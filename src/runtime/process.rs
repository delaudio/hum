use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};

use super::registry::{inspect_identity, IdentityStatus, RuntimeEntry};

#[derive(Debug, Clone, Copy)]
pub struct DetachedProcess {
    pub pid: u32,
    pub pgid: i32,
    pub log_sink_pid: Option<u32>,
    pub log_sink_start_time: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachedStopOutcome {
    Stopped,
    AlreadyMissing,
    IdentityMismatch(String),
}

/// Spawn a service in a new session with file-backed stdio. The returned
/// process is intentionally not owned by the current Tokio runtime.
pub fn spawn_detached(
    command: &str,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    identity_file: &std::fs::File,
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
    log_policy: super::logs::LogPolicy,
) -> Result<DetachedProcess> {
    use std::process::Command as StdCommand;

    #[cfg(unix)]
    let (stdout_reader, stdout_writer) =
        std::os::unix::net::UnixStream::pair().context("failed to create stdout log pipe")?;
    #[cfg(unix)]
    let (stderr_reader, stderr_writer) =
        std::os::unix::net::UnixStream::pair().context("failed to create stderr log pipe")?;
    #[cfg(not(unix))]
    anyhow::bail!("detached log capture is currently supported on macOS and Linux");

    #[cfg(all(unix, not(test)))]
    let mut sink = {
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;

        let stderr_fd = stderr_reader.as_raw_fd();
        let executable = std::env::current_exe().context("failed to locate hum log sink")?;
        let mut sink = StdCommand::new(executable);
        sink.args(super::logs::internal_sink_args(
            stdout_path,
            stderr_path,
            log_policy,
        ))
        .stdin(Stdio::from(OwnedFd::from(stdout_reader)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        unsafe {
            sink.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(stderr_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        sink.spawn().context("failed to start detached log sink")?
    };
    #[cfg(all(unix, not(test)))]
    drop(stderr_reader);
    #[cfg(all(unix, test))]
    let sink = super::logs::spawn_test_sink(
        stdout_reader,
        stderr_reader,
        stdout_path.to_path_buf(),
        stderr_path.to_path_buf(),
        log_policy,
    );
    #[cfg(not(test))]
    let (log_sink_pid, log_sink_start_time) = {
        let pid = sink.id();
        (Some(pid), Some(super::registry::process_start_time(pid)?))
    };
    #[cfg(test)]
    let (log_sink_pid, log_sink_start_time) = (None, None);

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    make_inheritable(identity_file)?;
    let mut process = StdCommand::new(shell);
    process
        .arg("-c")
        // Stop after creating the session but before evaluating user code so
        // the parent can persist identity metadata without an exit race.
        .arg("kill -STOP $$; eval \"$1\"")
        .arg("hum-detached")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(stdout_writer)))
        .stderr(Stdio::from(std::os::fd::OwnedFd::from(stderr_writer)));

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        process.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let child = process
        .spawn()
        .with_context(|| format!("failed to start detached command `{command}`"));
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            drop(process);
            #[cfg(not(test))]
            let _ = sink.wait();
            #[cfg(test)]
            let _ = sink.join();
            return Err(error);
        }
    };
    let pid = child.id();
    if let Err(error) = wait_until_stopped(pid) {
        let _ = send_group_signal(pid as i32, libc::SIGKILL);
        let _ = child.wait();
        return Err(error);
    }
    drop(child);

    #[cfg(unix)]
    let pgid = pid as i32;
    #[cfg(not(unix))]
    let pgid = pid as i32;

    Ok(DetachedProcess {
        pid,
        pgid,
        log_sink_pid,
        log_sink_start_time,
    })
}

#[cfg(unix)]
fn wait_until_stopped(pid: u32) -> Result<()> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(pid as i32, &mut status, libc::WUNTRACED) };
        if result == pid as i32 {
            if libc::WIFSTOPPED(status) {
                return Ok(());
            }
            anyhow::bail!("detached bootstrap PID {pid} exited before registration");
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("failed waiting for detached bootstrap to stop");
        }
    }
}

#[cfg(not(unix))]
fn wait_until_stopped(_pid: u32) -> Result<()> {
    anyhow::bail!("detached bootstrap is currently supported on macOS and Linux")
}

#[cfg(unix)]
fn make_inheritable(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        Err(std::io::Error::last_os_error()).context("failed to inherit runtime identity lock")
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn make_inheritable(_file: &std::fs::File) -> Result<()> {
    anyhow::bail!("detached process identity is currently supported on macOS and Linux")
}

pub async fn stop_detached(entry: &RuntimeEntry, grace: Duration) -> Result<DetachedStopOutcome> {
    match inspect_identity(entry) {
        IdentityStatus::Missing => {
            return confirm_log_sink_exit(entry, DetachedStopOutcome::AlreadyMissing).await;
        }
        IdentityStatus::Mismatch(reason) => {
            return Ok(DetachedStopOutcome::IdentityMismatch(reason));
        }
        IdentityStatus::Matching => {}
    }

    send_group_signal(entry.pgid, libc::SIGTERM)?;
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        match inspect_identity(entry) {
            IdentityStatus::Missing => {
                return confirm_log_sink_exit(entry, DetachedStopOutcome::Stopped).await;
            }
            IdentityStatus::Mismatch(reason) => {
                return Ok(DetachedStopOutcome::IdentityMismatch(reason));
            }
            IdentityStatus::Matching if tokio::time::Instant::now() >= deadline => break,
            IdentityStatus::Matching => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

    // Re-check identity immediately before the destructive signal.
    match inspect_identity(entry) {
        IdentityStatus::Matching => send_group_signal(entry.pgid, libc::SIGKILL)?,
        IdentityStatus::Missing => {
            return confirm_log_sink_exit(entry, DetachedStopOutcome::Stopped).await;
        }
        IdentityStatus::Mismatch(reason) => {
            return Ok(DetachedStopOutcome::IdentityMismatch(reason));
        }
    }
    let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match inspect_identity(entry) {
            IdentityStatus::Missing => {
                return confirm_log_sink_exit(entry, DetachedStopOutcome::Stopped).await;
            }
            IdentityStatus::Mismatch(reason) => {
                return Ok(DetachedStopOutcome::IdentityMismatch(reason));
            }
            IdentityStatus::Matching if tokio::time::Instant::now() >= kill_deadline => {
                anyhow::bail!(
                    "process group {} is still alive after SIGKILL confirmation timeout",
                    entry.pgid
                );
            }
            IdentityStatus::Matching => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}

async fn confirm_log_sink_exit(
    entry: &RuntimeEntry,
    outcome: DetachedStopOutcome,
) -> Result<DetachedStopOutcome> {
    wait_log_sink_exit(entry).await?;
    Ok(outcome)
}

pub async fn wait_log_sink_exit(entry: &RuntimeEntry) -> Result<()> {
    let Some((pid, start_time)) = entry.log_sink_pid.zip(entry.log_sink_start_time) else {
        return Ok(());
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if !process_matches_start_time(pid, start_time) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("log sink PID {pid} did not exit after service streams closed");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn process_matches_start_time(pid: u32, start_time: u64) -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let process_pid = sysinfo::Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[process_pid]),
        true,
        ProcessRefreshKind::new(),
    );
    system
        .process(process_pid)
        .is_some_and(|process| process.start_time() == start_time)
}

/// Emergency cleanup used only before a newly spawned process has enough
/// identity metadata to enter the persistent registry.
pub fn abort_unregistered(process: DetachedProcess) -> Result<()> {
    send_group_signal(process.pgid, libc::SIGKILL)
}

pub fn resume_detached(process: DetachedProcess) -> Result<()> {
    send_group_signal(process.pgid, libc::SIGCONT)
}

#[cfg(unix)]
fn send_group_signal(pgid: i32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error.into())
        }
    }
}

#[cfg(not(unix))]
fn send_group_signal(_pgid: i32, _signal: i32) -> Result<()> {
    anyhow::bail!("detached process groups are currently supported on macOS and Linux")
}
