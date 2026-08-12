use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    Ran,
    AlreadySatisfied,
}

pub struct TaskRunner {
    config: Config,
    root_dir: PathBuf,
    env_overrides: HashMap<String, String>,
}

impl TaskRunner {
    pub fn new(config: Config, root_dir: PathBuf, env_overrides: HashMap<String, String>) -> Self {
        Self {
            config,
            root_dir,
            env_overrides,
        }
    }

    pub async fn run(&self, name: &str) -> Result<TaskOutcome> {
        let task = self
            .config
            .tasks
            .get(name)
            .ok_or_else(|| anyhow!("unknown task '{name}'"))?;
        let cwd = task
            .cwd
            .as_ref()
            .map(|path| absolute_from(&self.root_dir, path))
            .unwrap_or_else(|| self.root_dir.clone());
        let environment = crate::config::environment::resolve_task_env_with_providers(
            &self.config,
            task,
            &cwd,
            &self.root_dir,
            &self.env_overrides,
        )
        .await?;

        if let Some(check) = &task.check {
            if run_check(check, &cwd, &environment).await {
                return Ok(TaskOutcome::AlreadySatisfied);
            }
        }

        let mut child = task_command(&task.command, &cwd, &environment)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start task '{name}'"))?;
        let status = match tokio::time::timeout(task.timeout, child.wait()).await {
            Ok(status) => status.with_context(|| format!("failed to wait for task '{name}'"))?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(anyhow!(
                    "task '{name}' exceeded timeout {}",
                    humantime::format_duration(task.timeout)
                ));
            }
        };
        if !status.success() {
            return Err(anyhow!("task '{name}' failed with status {status}"));
        }
        Ok(TaskOutcome::Ran)
    }
}

async fn run_check(argv: &[String], cwd: &Path, environment: &HashMap<String, String>) -> bool {
    task_command(argv, cwd, environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn task_command(argv: &[String], cwd: &Path, environment: &HashMap<String, String>) -> Command {
    let program = Path::new(&argv[0]);
    let program = if program.components().count() > 1 {
        absolute_from(cwd, program)
    } else {
        program.to_path_buf()
    };
    let mut command = Command::new(program);
    command.args(&argv[1..]).current_dir(cwd).envs(environment);
    command
}

fn absolute_from(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::TaskConfig;

    #[tokio::test]
    #[cfg(unix)]
    async fn task_uses_direct_argv_and_an_idempotent_check() {
        let root = std::env::temp_dir().join(format!(
            "hum-task-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("scripts")).unwrap();
        let script = root.join("scripts/setup");
        fs::write(&script, "#!/bin/sh\nset -eu\nprintf '%s' \"$1\" > marker\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let config = Config {
            version: 3,
            tasks: HashMap::from([(
                "setup".to_string(),
                TaskConfig {
                    command: vec![
                        "./scripts/setup".to_string(),
                        "argument with spaces".to_string(),
                    ],
                    check: Some(vec![
                        "test".to_string(),
                        "-f".to_string(),
                        "marker".to_string(),
                    ]),
                    doctor: None,
                    cwd: None,
                    env: HashMap::new(),
                    env_from: Vec::new(),
                    depends_on: Vec::new(),
                    timeout: std::time::Duration::from_secs(5),
                },
            )]),
            ..Config::default()
        };
        let runner = TaskRunner::new(config, root.clone(), HashMap::new());
        assert_eq!(runner.run("setup").await.unwrap(), TaskOutcome::Ran);
        assert_eq!(
            fs::read_to_string(root.join("marker")).unwrap(),
            "argument with spaces"
        );
        assert_eq!(
            runner.run("setup").await.unwrap(),
            TaskOutcome::AlreadySatisfied
        );
        fs::remove_dir_all(root).unwrap();
    }
}
