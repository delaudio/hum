use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use super::logs::{LogBuffer, LogLine, Stream as LogStream};
use super::registry::{inspect_identity, IdentityStatus, RuntimeEntry};

/// A running (or just-exited) child process, owned by the `hum` session
/// (section 15.1) and started in its own process group (section 15.2) so
/// that descendants spawned by e.g. `pnpm dev` can be terminated together.
pub struct RunningProcess {
    pub pid: i32,
    child: tokio::sync::Mutex<Option<Child>>,
    pub exit_code: AtomicI32,
    pub exited: Arc<tokio::sync::Notify>,
    pub has_exited: std::sync::atomic::AtomicBool,
}

pub const NO_EXIT_CODE: i32 = i32::MIN;

#[derive(Debug, Clone, Copy)]
pub struct DetachedProcess {
    pub pid: u32,
    pub pgid: i32,
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
) -> Result<DetachedProcess> {
    use std::fs::OpenOptions;
    use std::process::Command as StdCommand;

    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(stdout_path)
        .with_context(|| format!("failed to open {}", stdout_path.display()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(stderr_path)
        .with_context(|| format!("failed to open {}", stderr_path.display()))?;
    restrict_log_file(&stdout)?;
    restrict_log_file(&stderr)?;

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
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

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

    let mut child = process
        .spawn()
        .with_context(|| format!("failed to start detached command `{command}`"))?;
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

    Ok(DetachedProcess { pid, pgid })
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
        IdentityStatus::Missing => return Ok(DetachedStopOutcome::AlreadyMissing),
        IdentityStatus::Mismatch(reason) => {
            return Ok(DetachedStopOutcome::IdentityMismatch(reason));
        }
        IdentityStatus::Matching => {}
    }

    send_group_signal(entry.pgid, libc::SIGTERM)?;
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        match inspect_identity(entry) {
            IdentityStatus::Missing | IdentityStatus::Mismatch(_) => {
                return Ok(DetachedStopOutcome::Stopped);
            }
            IdentityStatus::Matching if tokio::time::Instant::now() >= deadline => break,
            IdentityStatus::Matching => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

    // Re-check identity immediately before the destructive signal.
    match inspect_identity(entry) {
        IdentityStatus::Matching => send_group_signal(entry.pgid, libc::SIGKILL)?,
        IdentityStatus::Missing | IdentityStatus::Mismatch(_) => {
            return Ok(DetachedStopOutcome::Stopped);
        }
    }
    Ok(DetachedStopOutcome::Stopped)
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

#[cfg(unix)]
fn restrict_log_file(file: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_log_file(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

impl RunningProcess {
    /// RF-03: spawn `command` (via the user's shell) in `cwd`, with `env`
    /// applied on top of the inherited environment.
    pub fn spawn(
        service: &str,
        command: &str,
        cwd: &std::path::Path,
        env: &std::collections::HashMap<String, String>,
        logs: Arc<LogBuffer>,
    ) -> Result<Arc<RunningProcess>> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = Command::new(shell);
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(false);

        #[cfg(unix)]
        {
            // New process group whose pgid == the child's pid, so we can
            // signal the whole tree with `kill(-pgid, sig)`.
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start service '{service}': `{command}`"))?;
        let pid = child.id().context("child has no pid")? as i32;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let proc = Arc::new(RunningProcess {
            pid,
            child: tokio::sync::Mutex::new(Some(child)),
            exit_code: AtomicI32::new(NO_EXIT_CODE),
            exited: Arc::new(tokio::sync::Notify::new()),
            has_exited: std::sync::atomic::AtomicBool::new(false),
        });

        if let Some(stdout) = stdout {
            spawn_log_reader(service.to_string(), stdout, LogStream::Stdout, logs.clone());
        }
        if let Some(stderr) = stderr {
            spawn_log_reader(service.to_string(), stderr, LogStream::Stderr, logs.clone());
        }

        spawn_waiter(proc.clone(), service.to_string(), logs);

        Ok(proc)
    }

    /// RF-07: graceful termination first (SIGTERM to the whole process
    /// group), then SIGKILL after `grace` if it hasn't exited.
    pub async fn stop(&self, grace: Duration) -> Result<()> {
        if self.has_exited.load(Ordering::SeqCst) {
            return Ok(());
        }

        #[cfg(unix)]
        {
            send_signal(self.pid, libc::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            if let Some(child) = self.child.lock().await.as_mut() {
                let _ = child.start_kill();
            }
        }

        let waited = timeout(grace, self.exited.notified()).await;
        if waited.is_err() && !self.has_exited.load(Ordering::SeqCst) {
            #[cfg(unix)]
            {
                send_signal(self.pid, libc::SIGKILL);
            }
            #[cfg(not(unix))]
            {
                if let Some(child) = self.child.lock().await.as_mut() {
                    let _ = child.start_kill();
                }
            }
            let _ = timeout(Duration::from_secs(3), self.exited.notified()).await;
        }
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        !self.has_exited.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<i32> {
        let code = self.exit_code.load(Ordering::SeqCst);
        if code == NO_EXIT_CODE {
            None
        } else {
            Some(code)
        }
    }
}

#[cfg(unix)]
fn send_signal(pid: i32, sig: i32) {
    unsafe {
        // Negative pid targets the whole process group (section 15.2).
        libc::kill(-pid, sig);
    }
}

fn spawn_log_reader<R>(service: String, reader: R, stream: LogStream, logs: Arc<LogBuffer>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(content)) => {
                    logs.push(LogLine {
                        timestamp: Local::now(),
                        service: service.clone(),
                        stream,
                        content,
                    });
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

fn spawn_waiter(proc: Arc<RunningProcess>, service: String, logs: Arc<LogBuffer>) {
    tokio::spawn(async move {
        let status = {
            let mut guard = proc.child.lock().await;
            match guard.as_mut() {
                Some(child) => child.wait().await,
                None => return,
            }
        };
        let code = match status {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        proc.exit_code.store(code, Ordering::SeqCst);
        proc.has_exited.store(true, Ordering::SeqCst);
        logs.push(LogLine {
            timestamp: Local::now(),
            service,
            stream: LogStream::System,
            content: format!("process exited with code {code}"),
        });
        proc.exited.notify_waiters();
    });
}
