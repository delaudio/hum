use std::io::{Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::registry::{inspect_identity, IdentityStatus, RuntimeEntry};

#[cfg(all(unix, not(test)))]
const LOG_EXPORT_CONFIG_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(all(unix, not(test)))]
const LOG_SINK_READY_TIMEOUT: Duration = Duration::from_secs(3);

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

pub struct DetachedOutput<'a> {
    pub exit_code_path: &'a std::path::Path,
    pub stdout_path: &'a std::path::Path,
    pub stderr_path: &'a std::path::Path,
    pub log_policy: super::logs::LogPolicy,
    pub log_export: super::logs::LogExportSpec,
}

fn serialize_log_export_config(export: &super::logs::LogExportSpec) -> Result<Vec<u8>> {
    let serialized =
        serde_json::to_vec(export).context("failed to serialize log export configuration")?;
    if serialized.len() > super::logs::MAX_EXPORT_SPEC_BYTES {
        anyhow::bail!(
            "log export configuration is {} bytes; maximum supported size is {} bytes",
            serialized.len(),
            super::logs::MAX_EXPORT_SPEC_BYTES
        );
    }
    Ok(serialized)
}

#[cfg(unix)]
fn anonymous_log_export_channel() -> Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    std::os::unix::net::UnixStream::pair().context("failed to create anonymous log export channel")
}

#[cfg(unix)]
fn send_log_export_config(
    writer: &mut std::os::unix::net::UnixStream,
    mut serialized: &[u8],
    timeout: Duration,
) -> Result<bool> {
    use std::os::fd::AsRawFd;

    writer
        .set_nonblocking(true)
        .context("failed to configure anonymous log export channel")?;
    let deadline = Instant::now() + timeout;
    while !serialized.is_empty() {
        match writer.write(serialized) {
            Ok(0) => anyhow::bail!("anonymous log export channel closed before configuration"),
            Ok(written) => serialized = &serialized[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    writer.shutdown(std::net::Shutdown::Write).ok();
                    return Ok(false);
                };
                let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut descriptor = libc::pollfd {
                    fd: writer.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
                if ready == 0 {
                    writer.shutdown(std::net::Shutdown::Write).ok();
                    return Ok(false);
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("failed waiting for anonymous log export channel");
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    anyhow::bail!("anonymous log export channel became unavailable");
                }
            }
            Err(error) => {
                return Err(error).context("failed to send anonymous log export configuration")
            }
        }
    }
    writer
        .shutdown(std::net::Shutdown::Write)
        .context("failed to finish anonymous log export configuration")?;
    Ok(true)
}

#[cfg(unix)]
fn wait_for_log_sink_ready(
    reader: &mut std::os::unix::net::UnixStream,
    timeout: Duration,
) -> Result<bool> {
    use std::os::fd::AsRawFd;

    reader
        .set_nonblocking(true)
        .context("failed to configure anonymous log sink readiness channel")?;
    let deadline = Instant::now() + timeout;
    let mut acknowledgement = [0_u8; 1];
    loop {
        match reader.read(&mut acknowledgement) {
            Ok(1) if acknowledgement[0] == super::logs::INTERNAL_SINK_EXPORT_READY => {
                return Ok(true);
            }
            Ok(1) => {
                anyhow::bail!("detached log sink returned an invalid readiness acknowledgement")
            }
            Ok(0) => anyhow::bail!("detached log sink closed before readiness acknowledgement"),
            Ok(_) => unreachable!("read buffer contains exactly one byte"),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return Ok(false);
                };
                let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut descriptor = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
                if ready == 0 {
                    return Ok(false);
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("failed waiting for detached log sink readiness");
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    anyhow::bail!("anonymous log export channel failed before sink readiness");
                }
            }
            Err(error) => return Err(error).context("failed to read detached log sink readiness"),
        }
    }
}

fn report_log_export_config_timeout(writer: &mut dyn Write) {
    let _ = writeln!(
        writer,
        "hum warning: timed out sending private log export configuration; the service will start with HTTP log export disabled"
    );
}

fn report_log_export_config_failure(writer: &mut dyn Write) {
    let _ = writeln!(
        writer,
        "hum warning: private log export channel failed; log capture is ready with HTTP export disabled"
    );
}

#[cfg(unix)]
fn duplicate_internal_sink_source(fd: std::os::fd::RawFd) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let minimum = super::logs::INTERNAL_SINK_READY_FD + 1;
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to reserve collision-free internal sink descriptor");
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(all(unix, not(test)))]
fn spawn_log_sink_child(
    stdout_reader: std::os::unix::net::UnixStream,
    stderr_reader: std::os::unix::net::UnixStream,
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
    log_policy: super::logs::LogPolicy,
) -> Result<(
    std::process::Child,
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let stderr_source = duplicate_internal_sink_source(stderr_reader.as_raw_fd())?;
    let stderr_fd = stderr_source.as_raw_fd();
    let (export_reader, export_writer) = anonymous_log_export_channel()?;
    let export_source = duplicate_internal_sink_source(export_reader.as_raw_fd())?;
    let export_fd = export_source.as_raw_fd();
    let (ready_reader, ready_writer) = anonymous_log_export_channel()?;
    let ready_source = duplicate_internal_sink_source(ready_writer.as_raw_fd())?;
    let ready_fd = ready_source.as_raw_fd();
    let executable = std::env::current_exe().context("failed to locate hum log sink")?;
    let mut sink = std::process::Command::new(executable);
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
            if libc::dup2(stderr_fd, super::logs::INTERNAL_SINK_STDERR_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(export_fd, super::logs::INTERNAL_SINK_EXPORT_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(ready_fd, super::logs::INTERNAL_SINK_READY_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in [
                super::logs::INTERNAL_SINK_STDERR_FD,
                super::logs::INTERNAL_SINK_EXPORT_FD,
                super::logs::INTERNAL_SINK_READY_FD,
            ] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = sink.spawn().context("failed to start detached log sink")?;
    drop((stderr_source, export_source, ready_source));
    drop(stderr_reader);
    drop(export_reader);
    drop(ready_writer);
    Ok((child, export_writer, ready_reader))
}

/// Spawn a service in a new session with file-backed stdio. The returned
/// process is intentionally not owned by the current Tokio runtime.
pub fn spawn_detached(
    command: &str,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    identity_file: &std::fs::File,
    output: DetachedOutput<'_>,
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
        let export_config = serialize_log_export_config(&output.log_export)?;
        let (mut child, mut export_writer, mut ready_reader) = spawn_log_sink_child(
            stdout_reader,
            stderr_reader,
            output.stdout_path,
            output.stderr_path,
            output.log_policy,
        )?;
        let config_delivered = send_log_export_config(
            &mut export_writer,
            &export_config,
            LOG_EXPORT_CONFIG_WRITE_TIMEOUT,
        );
        drop(export_writer);
        let ready = wait_for_log_sink_ready(&mut ready_reader, LOG_SINK_READY_TIMEOUT);
        drop(ready_reader);
        match ready {
            Ok(true) => {
                if !matches!(config_delivered, Ok(true)) {
                    let mut stderr = std::io::stderr().lock();
                    if config_delivered.is_ok() {
                        report_log_export_config_timeout(&mut stderr);
                    } else {
                        report_log_export_config_failure(&mut stderr);
                    }
                }
                child
            }
            Ok(false) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("detached log sink readiness timed out")
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("detached log sink readiness failed");
            }
        }
    };
    #[cfg(all(unix, test))]
    let sink = super::logs::spawn_test_sink(
        stdout_reader,
        stderr_reader,
        output.stdout_path.to_path_buf(),
        output.stderr_path.to_path_buf(),
        output.log_policy,
        output.log_export,
    );
    #[cfg(not(test))]
    let (log_sink_pid, log_sink_start_time) = {
        let pid = sink.id();
        (Some(pid), Some(super::registry::process_start_time(pid)?))
    };
    #[cfg(test)]
    let (log_sink_pid, log_sink_start_time) = (None, None);

    // This is the sole construction point for the target service process.
    // Reaching it proves that primary or fail-safe log capture was prepared
    // first; no service can write into the pipes during sink replacement.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    make_inheritable(identity_file)?;
    let mut process = StdCommand::new(shell);
    process
        .arg("-c")
        // Stop after creating the session but before evaluating user code so
        // the parent can persist identity metadata without an exit race.
        .arg(
            "hum_exit_file=$2; trap 'hum_status=$?; trap - EXIT; printf \"%s\\n\" \"$hum_status\" > \"$hum_exit_file\"; exit \"$hum_status\"' EXIT; trap 'exit 143' TERM; trap 'exit 130' INT; trap 'exit 129' HUP; kill -STOP $$; eval \"$1\"",
        )
        .arg("hum-detached")
        .arg(command)
        .arg(output.exit_code_path)
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
    system.process(process_pid).is_some_and(|process| {
        process.start_time() == start_time
            && !matches!(
                process.status(),
                sysinfo::ProcessStatus::Dead | sysinfo::ProcessStatus::Zombie
            )
    })
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn serialized_log_export_configuration_is_bounded_before_spawn() {
        let export = super::super::logs::LogExportSpec {
            project: "x".repeat(super::super::logs::MAX_EXPORT_SPEC_BYTES),
            service: "api".to_string(),
            max_line_bytes: 64,
            redact_patterns: Vec::new(),
            exporters: Vec::new(),
        };

        assert!(serialize_log_export_config(&export).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn log_export_configuration_uses_a_pathless_socket() {
        let (mut reader, mut writer) = anonymous_log_export_channel().unwrap();
        assert!(reader.local_addr().unwrap().as_pathname().is_none());
        assert!(reader.peer_addr().unwrap().as_pathname().is_none());
        let fully_sent = send_log_export_config(
            &mut writer,
            b"private export configuration",
            Duration::from_secs(1),
        )
        .unwrap();
        let mut contents = String::new();
        reader.read_to_string(&mut contents).unwrap();

        assert!(fully_sent);
        assert_eq!(contents, "private export configuration");
    }

    #[cfg(unix)]
    #[test]
    fn log_export_configuration_send_has_one_wall_clock_deadline() {
        let (_reader, mut writer) = anonymous_log_export_channel().unwrap();
        let oversized_for_socket = vec![b'x'; super::super::logs::MAX_EXPORT_SPEC_BYTES];
        let started = Instant::now();

        let fully_sent = send_log_export_config(
            &mut writer,
            &oversized_for_socket,
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(!fully_sent);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn closed_log_export_configuration_channel_is_a_hard_send_error() {
        let (reader, mut writer) = anonymous_log_export_channel().unwrap();
        drop(reader);

        let result = send_log_export_config(
            &mut writer,
            b"private export configuration",
            Duration::from_secs(1),
        );

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn internal_sink_sources_are_reserved_above_all_fixed_targets() {
        use std::os::fd::AsRawFd;

        let (first, _first_peer) = anonymous_log_export_channel().unwrap();
        let (second, _second_peer) = anonymous_log_export_channel().unwrap();
        let (third, _third_peer) = anonymous_log_export_channel().unwrap();
        let duplicates = [
            duplicate_internal_sink_source(first.as_raw_fd()).unwrap(),
            duplicate_internal_sink_source(second.as_raw_fd()).unwrap(),
            duplicate_internal_sink_source(third.as_raw_fd()).unwrap(),
        ];
        let descriptors = duplicates
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(descriptors.len(), 3);
        assert!(descriptors
            .iter()
            .all(|fd| *fd > super::super::logs::INTERNAL_SINK_READY_FD));
    }

    #[cfg(unix)]
    #[test]
    fn log_sink_readiness_uses_an_independent_pathless_socket() {
        let (mut reader, mut writer) = anonymous_log_export_channel().unwrap();
        assert!(reader.local_addr().unwrap().as_pathname().is_none());
        writer
            .write_all(&[super::super::logs::INTERNAL_SINK_EXPORT_READY])
            .unwrap();

        assert!(wait_for_log_sink_ready(&mut reader, Duration::from_secs(1)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn log_sink_readiness_wait_has_a_bounded_timeout() {
        let (mut reader, _writer) = anonymous_log_export_channel().unwrap();
        let started = Instant::now();

        let ready = wait_for_log_sink_ready(&mut reader, Duration::from_millis(20)).unwrap();

        assert!(!ready);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn log_export_configuration_timeout_warning_is_static_and_actionable() {
        let mut output = Vec::new();

        report_log_export_config_timeout(&mut output);

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "hum warning: timed out sending private log export configuration; the service will start with HTTP log export disabled\n"
        );
    }

    #[test]
    fn log_export_configuration_failure_warning_is_static_and_actionable() {
        let mut output = Vec::new();

        report_log_export_config_failure(&mut output);

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "hum warning: private log export channel failed; log capture is ready with HTTP export disabled\n"
        );
    }
}
