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
