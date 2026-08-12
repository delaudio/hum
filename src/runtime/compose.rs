use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use crate::config::{Config, RuntimeConfig};
use crate::core::state::{HealthState, PortState, ProcessState};

use super::adapter::{AdapterFuture, RuntimeAdapter};
use super::detached::{DetachedServiceStatus, StartReport, StopFailure, StopReport};
use super::health;
use super::portcheck::{self, PortProbe};
use super::registry::RuntimeRegistry;

#[derive(Debug)]
pub struct ComposeRuntime {
    name: String,
    project: String,
    project_name: String,
    files: Vec<PathBuf>,
    reconcile: bool,
    generated_files: Vec<PathBuf>,
    profiles: Vec<String>,
    env_file: Option<PathBuf>,
    root_dir: PathBuf,
    generated_override: PathBuf,
    config: Config,
    env_overrides: HashMap<String, String>,
}

#[derive(Serialize)]
struct GeneratedOverride {
    services: BTreeMap<String, GeneratedService>,
}

#[derive(Serialize)]
struct GeneratedService {
    environment: BTreeMap<String, String>,
}

impl ComposeRuntime {
    pub fn new(
        name: String,
        project: String,
        config: Config,
        root_dir: PathBuf,
        env_overrides: HashMap<String, String>,
    ) -> Result<Self> {
        let RuntimeConfig::Compose {
            project_name,
            files,
            reconcile,
            generated_files,
            profiles,
            env_file,
        } = config
            .runtimes
            .get(&name)
            .ok_or_else(|| anyhow!("unknown runtime '{name}'"))?
            .clone()
        else {
            return Err(anyhow!("runtime '{name}' is not a Compose runtime"));
        };
        let state = RuntimeRegistry::for_project(&project)?;
        let generated_directory = state.root().join("compose");
        std::fs::create_dir_all(&generated_directory).with_context(|| {
            format!(
                "failed to create Compose runtime directory {}",
                generated_directory.display()
            )
        })?;
        let generated_override = generated_directory.join(format!("{name}.generated.yaml"));
        Ok(Self {
            name,
            project,
            project_name,
            files: files
                .into_iter()
                .map(|file| absolute_from(&root_dir, file))
                .collect(),
            reconcile,
            generated_files: generated_files
                .into_iter()
                .map(|file| absolute_from(&root_dir, file))
                .collect(),
            profiles,
            env_file: env_file.map(|file| absolute_from(&root_dir, file)),
            root_dir,
            generated_override,
            config,
            env_overrides,
        })
    }

    fn target<'a>(&'a self, service: &str) -> Result<&'a str> {
        let service = self
            .config
            .services
            .get(service)
            .ok_or_else(|| anyhow!("unknown service '{service}'"))?;
        if service.runtime.as_deref() != Some(self.name.as_str()) {
            return Err(anyhow!(
                "service is not owned by Compose runtime '{}'",
                self.name
            ));
        }
        service
            .target
            .as_deref()
            .ok_or_else(|| anyhow!("Compose service has no target"))
    }

    fn base_arguments(&self, include_generated: bool) -> Vec<String> {
        let mut arguments = vec![
            "compose".to_string(),
            "--project-name".to_string(),
            self.project_name.clone(),
        ];
        for file in &self.files {
            arguments.push("--file".to_string());
            arguments.push(file.display().to_string());
        }
        for file in &self.generated_files {
            if file.is_file() {
                arguments.push("--file".to_string());
                arguments.push(file.display().to_string());
            }
        }
        if include_generated && self.generated_override.is_file() {
            arguments.push("--file".to_string());
            arguments.push(self.generated_override.display().to_string());
        }
        if let Some(env_file) = &self.env_file {
            arguments.push("--env-file".to_string());
            arguments.push(env_file.display().to_string());
        }
        for profile in &self.profiles {
            arguments.push("--profile".to_string());
            arguments.push(profile.clone());
        }
        arguments
    }

    async fn prepare_environment(&self, services: &[String]) -> Result<HashMap<String, String>> {
        let mut environment = HashMap::new();
        let mut generated = GeneratedOverride {
            services: BTreeMap::new(),
        };
        for name in services {
            let service = self
                .config
                .services
                .get(name)
                .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
            let values = crate::config::environment::resolve_service_env_with_providers(
                &self.config,
                service,
                &self.root_dir,
                &self.root_dir,
                &self.env_overrides,
            )
            .await?;
            let target = self.target(name)?.to_string();
            let entry = generated
                .services
                .entry(target)
                .or_insert_with(|| GeneratedService {
                    environment: BTreeMap::new(),
                });
            for (key, value) in values {
                let scoped = scoped_environment_name(&self.project, &self.name, name, &key);
                entry.environment.insert(key, format!("${{{scoped}:-}}"));
                environment.insert(scoped, value);
            }
        }
        if generated
            .services
            .values()
            .any(|service| !service.environment.is_empty())
        {
            write_generated_override(&self.generated_override, &generated)?;
        } else if self.generated_override.exists() {
            std::fs::remove_file(&self.generated_override).with_context(|| {
                format!(
                    "failed to remove stale generated Compose override {}",
                    self.generated_override.display()
                )
            })?;
        }
        Ok(environment)
    }

    async fn run(&self, arguments: &[String], environment: &HashMap<String, String>) -> Result<()> {
        let status = Command::new("docker")
            .args(arguments)
            .envs(environment)
            .env("COMPOSE_IGNORE_ORPHANS", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("failed to execute Docker CLI")?;
        if !status.success() {
            return Err(anyhow!(
                "Docker Compose command failed with status {status}"
            ));
        }
        Ok(())
    }

    async fn capture_services(&self, status: Option<&str>) -> Result<HashSet<String>> {
        let mut arguments = self.base_arguments(true);
        arguments.push("ps".to_string());
        if let Some(status) = status {
            arguments.push("--status".to_string());
            arguments.push(status.to_string());
        } else {
            arguments.push("--all".to_string());
        }
        arguments.push("--services".to_string());
        let output = Command::new("docker")
            .args(&arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .context("failed to inspect Docker Compose services")?;
        if !output.status.success() {
            return Err(anyhow!("Docker Compose status inspection failed"));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| anyhow!("Docker Compose returned non-UTF-8 service names"))?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn start_owned(&self, services: &[String]) -> Result<StartReport> {
        let running = self.capture_services(Some("running")).await?;
        let mut report = StartReport::default();
        let mut targets = Vec::with_capacity(services.len());
        for name in services {
            let target = self.target(name)?;
            if running.contains(target) {
                if self.reconcile {
                    report.reconciled.push(name.clone());
                } else {
                    report.already_running.push(name.clone());
                }
            } else {
                report.started.push(name.clone());
            }
            if self.reconcile || !running.contains(target) {
                targets.push(target.to_string());
            }
        }
        if targets.is_empty() {
            return Ok(report);
        }
        let environment = self.prepare_environment(services).await?;
        let mut arguments = self.base_arguments(true);
        arguments.extend([
            "up".to_string(),
            "--detach".to_string(),
            "--wait".to_string(),
        ]);
        arguments.extend(targets);
        self.run(&arguments, &environment).await?;
        Ok(report)
    }

    async fn stop_owned(&self, services: &[String]) -> Result<StopReport> {
        let running = self.capture_services(Some("running")).await?;
        let mut report = StopReport::default();
        let mut targets = Vec::new();
        for name in services {
            match self.target(name) {
                Ok(target) if running.contains(target) => {
                    report.stopped.push(name.clone());
                    targets.push(target.to_string());
                }
                Ok(_) => report.already_stopped.push(name.clone()),
                Err(error) => report.failures.push(StopFailure {
                    service: name.clone(),
                    detail: error.to_string(),
                }),
            }
        }
        if !targets.is_empty() && report.failures.is_empty() {
            let mut arguments = self.base_arguments(true);
            arguments.push("stop".to_string());
            arguments.extend(targets);
            if let Err(error) = self.run(&arguments, &HashMap::new()).await {
                report.failures.push(StopFailure {
                    service: self.name.clone(),
                    detail: error.to_string(),
                });
                report.stopped.clear();
            }
        }
        Ok(report)
    }

    async fn status_owned(
        &self,
        services: &[String],
        check_health: bool,
    ) -> Result<Vec<DetachedServiceStatus>> {
        let running = self.capture_services(Some("running")).await?;
        let existing = self.capture_services(None).await?;
        let mut statuses = Vec::with_capacity(services.len());
        for name in services {
            let target = self.target(name)?;
            let service = &self.config.services[name];
            let process = if running.contains(target) {
                ProcessState::Running
            } else if existing.contains(target) {
                ProcessState::Exited
            } else {
                ProcessState::Missing
            };
            let port = match service.port {
                Some(port) if portcheck::probe_port(port) == PortProbe::Listening => {
                    PortState::ListeningUnverified
                }
                Some(_) => PortState::Closed,
                None => PortState::Unknown,
            };
            let health = match service
                .healthcheck
                .as_ref()
                .filter(|_| check_health && process == ProcessState::Running)
            {
                Some(check) if health::check_once(check).await.is_ok() => HealthState::Healthy,
                Some(_) => HealthState::Unhealthy,
                None => HealthState::Unchecked,
            };
            statuses.push(DetachedServiceStatus {
                name: name.clone(),
                process,
                port,
                health,
                configured_port: service.port,
                pid: None,
                pgid: None,
                exit_code: None,
                cwd: Some(self.root_dir.clone()),
                command: Some(format!("docker compose service {target}")),
                stdout_log: None,
                stderr_log: None,
                detail: Some(format!(
                    "Compose project {} via runtime {}",
                    self.project_name, self.name
                )),
                health_detail: None,
                health_duration_ms: None,
                started_at: None,
            });
        }
        Ok(statuses)
    }

    async fn stream_logs_owned(
        &self,
        services: &[String],
        lines: usize,
        follow: bool,
    ) -> Result<()> {
        let mut arguments = self.base_arguments(true);
        arguments.push("logs".to_string());
        arguments.push("--tail".to_string());
        arguments.push(lines.to_string());
        if follow {
            arguments.push("--follow".to_string());
        }
        for service in services {
            arguments.push(self.target(service)?.to_string());
        }
        let mut child = Command::new("docker")
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to stream Docker Compose logs")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture Docker Compose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture Docker Compose stderr"))?;
        let redactor = super::logs::Redactor::new(&self.config.logs.redact_patterns)?;
        let max_line_bytes = self.config.logs.max_line_bytes;
        let stdout_redactor = redactor.clone();
        let stderr_redactor = redactor;
        let (stdout_result, stderr_result, status) = tokio::join!(
            stream_redacted(stdout, tokio::io::stdout(), stdout_redactor, max_line_bytes),
            stream_redacted(stderr, tokio::io::stderr(), stderr_redactor, max_line_bytes),
            child.wait(),
        );
        stdout_result.context("failed to display Docker Compose stdout")?;
        stderr_result.context("failed to display Docker Compose stderr")?;
        let status = status.context("failed to wait for Docker Compose logs")?;
        if status.success() || status.code() == Some(130) {
            Ok(())
        } else {
            Err(anyhow!("Docker Compose logs failed with status {status}"))
        }
    }

    async fn capture_logs_owned(&self, services: &[String], lines: usize) -> Result<Vec<String>> {
        let mut arguments = self.base_arguments(true);
        arguments.extend([
            "logs".to_string(),
            "--no-color".to_string(),
            "--tail".to_string(),
            lines.to_string(),
        ]);
        for service in services {
            arguments.push(self.target(service)?.to_string());
        }
        let mut child = Command::new("docker")
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to capture Docker Compose logs")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture Docker Compose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture Docker Compose stderr"))?;
        let redactor = super::logs::Redactor::new(&self.config.logs.redact_patterns)?;
        let max_line_bytes = self.config.logs.max_line_bytes;
        let (stdout, stderr, status) = tokio::join!(
            capture_redacted_lines(stdout, redactor.clone(), max_line_bytes, lines.max(1)),
            capture_redacted_lines(stderr, redactor, max_line_bytes, 20),
            child.wait(),
        );
        let stdout = stdout.context("failed to capture Docker Compose stdout")?;
        let stderr = stderr.context("failed to capture Docker Compose stderr")?;
        let status = status.context("failed to wait for Docker Compose logs")?;
        if !status.success() {
            let detail = stderr.last().map_or("", String::as_str);
            return Err(anyhow!(
                "Docker Compose logs failed with status {status}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
        Ok(stdout)
    }

    async fn reset_owned(&self) -> Result<()> {
        let mut arguments = self.base_arguments(true);
        arguments.extend([
            "--profile".to_string(),
            "*".to_string(),
            "down".to_string(),
            "--volumes".to_string(),
            "--remove-orphans".to_string(),
        ]);
        self.run(&arguments, &HashMap::new()).await
    }
}

async fn stream_redacted<R, W>(
    mut reader: R,
    mut writer: W,
    redactor: super::logs::Redactor,
    max_line_bytes: usize,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut chunk = [0_u8; 8192];
    let mut line = Vec::with_capacity(max_line_bytes.min(chunk.len()));
    let mut oversized = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                write_redacted_line(&mut writer, &redactor, &line, oversized).await?;
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() < max_line_bytes {
                    line.push(*byte);
                } else {
                    line.clear();
                    oversized = true;
                }
            }
        }
    }
    if oversized || !line.is_empty() {
        write_redacted_line(&mut writer, &redactor, &line, oversized).await?;
    }
    writer.flush().await
}

async fn write_redacted_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    redactor: &super::logs::Redactor,
    line: &[u8],
    oversized: bool,
) -> std::io::Result<()> {
    let rendered = if oversized {
        "… [oversized log line omitted]".to_string()
    } else {
        redactor.redact(&String::from_utf8_lossy(line))
    };
    writer.write_all(rendered.as_bytes()).await?;
    writer.write_all(b"\n").await
}

async fn capture_redacted_lines<R: AsyncRead + Unpin>(
    mut reader: R,
    redactor: super::logs::Redactor,
    max_line_bytes: usize,
    max_lines: usize,
) -> std::io::Result<Vec<String>> {
    let mut chunk = [0_u8; 8192];
    let mut line = Vec::with_capacity(max_line_bytes.min(chunk.len()));
    let mut oversized = false;
    let mut captured = VecDeque::with_capacity(max_lines);
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                push_captured_line(&mut captured, &redactor, &line, oversized, max_lines);
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() < max_line_bytes {
                    line.push(*byte);
                } else {
                    line.clear();
                    oversized = true;
                }
            }
        }
    }
    if oversized || !line.is_empty() {
        push_captured_line(&mut captured, &redactor, &line, oversized, max_lines);
    }
    Ok(captured.into_iter().collect())
}

fn push_captured_line(
    captured: &mut VecDeque<String>,
    redactor: &super::logs::Redactor,
    line: &[u8],
    oversized: bool,
    max_lines: usize,
) {
    if captured.len() >= max_lines {
        captured.pop_front();
    }
    captured.push_back(if oversized {
        "… [oversized log line omitted]".to_string()
    } else {
        redactor.redact(&String::from_utf8_lossy(line))
    });
}

impl RuntimeAdapter for ComposeRuntime {
    fn owns_service(&self, service: &str) -> bool {
        self.config
            .services
            .get(service)
            .is_some_and(|service| service.runtime.as_deref() == Some(self.name.as_str()))
    }

    fn start_services<'a>(&'a self, services: &'a [String]) -> AdapterFuture<'a, StartReport> {
        Box::pin(self.start_owned(services))
    }

    fn stop_services<'a>(
        &'a self,
        services: &'a [String],
        _grace: Duration,
    ) -> AdapterFuture<'a, StopReport> {
        Box::pin(self.stop_owned(services))
    }

    fn status_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>> {
        Box::pin(self.status_owned(services, true))
    }

    fn monitor_services<'a>(
        &'a self,
        services: &'a [String],
    ) -> AdapterFuture<'a, Vec<DetachedServiceStatus>> {
        Box::pin(self.status_owned(services, false))
    }

    fn wait_ready<'a>(&'a self, _service: &'a str) -> AdapterFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn log_files(&self, service: &str) -> Result<Option<(PathBuf, PathBuf)>> {
        self.target(service)?;
        Ok(None)
    }

    fn stream_logs<'a>(
        &'a self,
        services: &'a [String],
        lines: usize,
        follow: bool,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(self.stream_logs_owned(services, lines, follow))
    }

    fn capture_logs<'a>(
        &'a self,
        services: &'a [String],
        lines: usize,
    ) -> AdapterFuture<'a, Vec<String>> {
        Box::pin(self.capture_logs_owned(services, lines))
    }

    fn reset<'a>(&'a self) -> AdapterFuture<'a, ()> {
        Box::pin(self.reset_owned())
    }
}

fn absolute_from(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn scoped_environment_name(project: &str, runtime: &str, service: &str, key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(project.as_bytes());
    digest.update([0]);
    digest.update(runtime.as_bytes());
    digest.update([0]);
    digest.update(service.as_bytes());
    let identity = format!("{:x}", digest.finalize());
    let key = key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("HUM_RUNTIME_{}_{}", &identity[..12], key)
}

fn write_generated_override(path: &Path, override_: &GeneratedOverride) -> Result<()> {
    let contents =
        yaml_serde::to_string(override_).context("failed to serialize Compose override")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("generated Compose override has no parent directory"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".hum-compose-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!(
                "failed to create generated Compose override near {}",
                path.display()
            )
        })?;
        file.write_all(contents.as_bytes())
            .context("failed to write generated Compose override")?;
        file.sync_all()
            .context("failed to sync generated Compose override")?;
        std::fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace generated Compose override {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(
                || {
                    format!(
                        "failed to protect generated Compose override {}",
                        path.display()
                    )
                },
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_environment_names_are_stable_and_product_agnostic() {
        let first = scoped_environment_name("demo", "infra", "api", "DATABASE_URL");
        let second = scoped_environment_name("demo", "infra", "api", "DATABASE_URL");
        assert_eq!(first, second);
        assert!(first.starts_with("HUM_RUNTIME_"));
        assert!(first.ends_with("_DATABASE_URL"));
        assert!(!first.to_lowercase().contains("demo"));
        assert_ne!(
            first,
            scoped_environment_name("another", "infra", "api", "DATABASE_URL")
        );
    }

    #[test]
    fn generated_override_contains_references_but_not_secret_values() {
        let root = std::env::temp_dir().join(format!(
            "hum-compose-override-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("generated.yaml");
        let override_ = GeneratedOverride {
            services: BTreeMap::from([(
                "api".to_string(),
                GeneratedService {
                    environment: BTreeMap::from([(
                        "TOKEN".to_string(),
                        "${HUM_RUNTIME_123_TOKEN:-}".to_string(),
                    )]),
                },
            )]),
        };
        write_generated_override(&path, &override_).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("HUM_RUNTIME_123_TOKEN"));
        assert!(!contents.contains("secret-value"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn captured_runtime_logs_are_redacted_line_and_memory_bounded() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            writer
                .write_all(b"old\ntoken=private\nthis-line-is-far-too-long\nlast\n")
                .await
                .unwrap();
        });
        let redactor = super::super::logs::Redactor::new(&["token=[^ ]+".to_string()]).unwrap();
        let lines = capture_redacted_lines(reader, redactor, 16, 3)
            .await
            .unwrap();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("[REDACTED]"));
        assert!(!lines[0].contains("private"));
        assert_eq!(lines[1], "… [oversized log line omitted]");
        assert_eq!(lines[2], "last");
    }
}
