use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{Config, HealthcheckConfig, Loaded, ReadyMode};
use crate::core::graph;

use super::health;
use super::portcheck;
use super::process::{self, DetachedStopOutcome};
use super::registry::{
    inspect_identity, new_runtime_token, process_start_time, IdentityStatus, RuntimeEntry,
    RuntimeRegistry,
};

const STOP_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
pub struct StartReport {
    pub started: Vec<String>,
    pub already_running: Vec<String>,
}

pub struct DetachedRuntime {
    project: String,
    config: Config,
    root_dir: PathBuf,
    env_overrides: HashMap<String, String>,
    registry: RuntimeRegistry,
}

impl DetachedRuntime {
    pub fn new(
        project: String,
        loaded: Loaded,
        env_overrides: HashMap<String, String>,
    ) -> Result<Self> {
        let registry = RuntimeRegistry::for_project(&project)?;
        Ok(Self {
            project,
            config: loaded.config,
            root_dir: loaded.root_dir,
            env_overrides,
            registry,
        })
    }

    #[cfg(test)]
    fn with_state_root(
        project: String,
        loaded: Loaded,
        env_overrides: HashMap<String, String>,
        state_root: PathBuf,
    ) -> Result<Self> {
        let registry = RuntimeRegistry::at(state_root, &project)?;
        Ok(Self {
            project,
            config: loaded.config,
            root_dir: loaded.root_dir,
            env_overrides,
            registry,
        })
    }

    pub fn registry(&self) -> &RuntimeRegistry {
        &self.registry
    }

    pub async fn start_template(&self, template: &str) -> Result<StartReport> {
        let order = graph::services_for_template(&self.config, template)?;
        self.start_ordered(&order).await
    }

    pub async fn start_services(&self, services: &[String]) -> Result<StartReport> {
        let order = graph::resolve_start_order(&self.config, services)?;
        self.start_ordered(&order).await
    }

    async fn start_ordered(&self, order: &[String]) -> Result<StartReport> {
        let registry = self.registry.clone();
        let _lock = tokio::task::spawn_blocking(move || registry.lock())
            .await
            .context("project lock task failed")??;
        let mut report = StartReport::default();
        let mut started_entries = Vec::new();
        let mut available = HashSet::new();

        for name in order {
            let service = self
                .config
                .services
                .get(name)
                .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
            for dependency in &service.depends_on {
                if !available.contains(dependency) {
                    let error = anyhow!(
                        "service '{name}' blocked because dependency '{dependency}' is unavailable"
                    );
                    return Err(self.with_rollback(error, &started_entries).await);
                }
                if let Err(error) = self.wait_ready(dependency).await {
                    let error = error.context(format!(
                        "service '{name}' dependency '{dependency}' did not become ready"
                    ));
                    return Err(self.with_rollback(error, &started_entries).await);
                }
            }

            match self.start_one(name).await {
                Ok(StartOne::Started(entry)) => {
                    report.started.push(name.clone());
                    started_entries.push(*entry);
                    available.insert(name.clone());
                }
                Ok(StartOne::AlreadyRunning) => {
                    report.already_running.push(name.clone());
                    available.insert(name.clone());
                }
                Err(error) => {
                    let error = error.context(format!("failed to start service '{name}'"));
                    return Err(self.with_rollback(error, &started_entries).await);
                }
            }
        }

        Ok(report)
    }

    async fn start_one(&self, name: &str) -> Result<StartOne> {
        let service = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let command = service
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("service '{name}' has no command"))?;
        let command_hash = stable_digest(command.as_bytes());
        let config_hash = config_digest(service)?;

        if let Some(entry) = self.registry.load(name)? {
            match inspect_identity(&entry) {
                IdentityStatus::Matching
                    if entry.command_hash == command_hash && entry.config_hash == config_hash =>
                {
                    return Ok(StartOne::AlreadyRunning);
                }
                IdentityStatus::Matching => {
                    return Err(anyhow!(
                        "service '{name}' is already running with a different command/config; restart it first"
                    ));
                }
                IdentityStatus::Missing | IdentityStatus::Mismatch(_) => {
                    self.registry.remove(name)?;
                }
            }
        }

        if let Some(port) = service.port {
            if let Some(occupant) = portcheck::check_port(port) {
                let owner = occupant
                    .pid
                    .map(|pid| format!("PID {pid}"))
                    .unwrap_or_else(|| "another process".to_string());
                return Err(anyhow!("port {port} is already occupied by {owner}"));
            }
        }

        let cwd = self.service_cwd(name)?;
        let env =
            crate::config::environment::resolve_service_env(service, &cwd, &self.env_overrides)?;
        let (stdout_log, stderr_log) = self.registry.log_paths(name);
        let runtime_token = new_runtime_token()?;
        let identity = self.registry.create_identity(name, &runtime_token)?;
        let process = process::spawn_detached(
            command,
            &cwd,
            &env,
            identity.file(),
            &stdout_log,
            &stderr_log,
        )?;
        let start_time = match process_start_time(process.pid) {
            Ok(start_time) => start_time,
            Err(error) => {
                let _ = process::abort_unregistered(process);
                return Err(error);
            }
        };
        let entry = RuntimeEntry::new(
            self.project.clone(),
            name.to_string(),
            process.pid,
            process.pgid,
            start_time,
            runtime_token,
            identity.path().to_path_buf(),
            command_hash,
            config_hash,
            service.port,
            cwd,
            stdout_log,
            stderr_log,
        );
        if let Err(error) = self.registry.write(&entry) {
            let _ = process::abort_unregistered(process);
            return Err(error);
        }
        if let Err(error) = process::resume_detached(process) {
            let _ = process::abort_unregistered(process);
            let _ = self.registry.remove(name);
            return Err(error.context("failed to resume detached service after registration"));
        }
        Ok(StartOne::Started(Box::new(entry)))
    }

    async fn wait_ready(&self, name: &str) -> Result<()> {
        let service = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let entry = self
            .registry
            .load(name)?
            .ok_or_else(|| anyhow!("runtime entry for '{name}' is missing"))?;
        let mode = service
            .depends_on_ready
            .unwrap_or(if service.healthcheck.is_some() {
                ReadyMode::Healthy
            } else if service.port.is_some() {
                ReadyMode::Listening
            } else {
                ReadyMode::Started
            });
        if mode == ReadyMode::Started {
            return ensure_matching(&entry, name);
        }

        let (attempts, interval) = match (&mode, &service.healthcheck) {
            (ReadyMode::Healthy, Some(check)) => {
                (health::retries(check).max(1), health::interval(check))
            }
            (ReadyMode::Listening, _) => (50, Duration::from_millis(200)),
            _ => {
                return Err(anyhow!(
                    "service '{name}' has invalid readiness configuration"
                ))
            }
        };
        for _ in 0..attempts {
            ensure_matching(&entry, name)?;
            let ready = match mode {
                ReadyMode::Started => true,
                ReadyMode::Listening => self.port_belongs_to(&entry),
                ReadyMode::Healthy => self.healthcheck_passes(service.healthcheck.as_ref()).await,
            };
            if ready {
                return Ok(());
            }
            tokio::time::sleep(interval).await;
        }
        Err(anyhow!(
            "service '{name}' did not become {}",
            readiness_label(mode)
        ))
    }

    fn port_belongs_to(&self, entry: &RuntimeEntry) -> bool {
        let Some(port) = entry.port else {
            return false;
        };
        if portcheck::probe_port(port) != portcheck::PortProbe::Listening {
            return false;
        }
        let occupant = portcheck::identify_occupant(port);
        occupant
            .pid
            .is_some_and(|pid| portcheck::belongs_to_process_tree(pid, entry.pid))
    }

    async fn healthcheck_passes(&self, healthcheck: Option<&HealthcheckConfig>) -> bool {
        match healthcheck {
            Some(check) => health::check_once(check).await.is_ok(),
            None => false,
        }
    }

    async fn rollback(&self, entries: &[RuntimeEntry]) -> Vec<String> {
        let mut failures = Vec::new();
        for entry in entries.iter().rev() {
            match process::stop_detached(entry, STOP_GRACE).await {
                Ok(DetachedStopOutcome::Stopped | DetachedStopOutcome::AlreadyMissing) => {
                    if let Err(error) = self.registry.remove(&entry.service) {
                        failures.push(format!("{} registry cleanup: {error}", entry.service));
                    }
                }
                Ok(DetachedStopOutcome::IdentityMismatch(reason)) => {
                    failures.push(format!("{} identity mismatch: {reason}", entry.service));
                }
                Err(error) => failures.push(format!("{} stop failed: {error}", entry.service)),
            }
        }
        failures
    }

    async fn with_rollback(&self, error: anyhow::Error, entries: &[RuntimeEntry]) -> anyhow::Error {
        let failures = self.rollback(entries).await;
        if failures.is_empty() {
            error
        } else {
            anyhow!("{error:#}; rollback incomplete: {}", failures.join("; "))
        }
    }

    fn service_cwd(&self, name: &str) -> Result<PathBuf> {
        let service = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let base = match &service.repository {
            Some(repository) => {
                let path = crate::config::loader::expand_home(
                    &self
                        .config
                        .repositories
                        .get(repository)
                        .ok_or_else(|| anyhow!("unknown repository '{repository}'"))?
                        .path,
                );
                if path.is_absolute() {
                    path
                } else {
                    self.root_dir.join(path)
                }
            }
            None => self.root_dir.clone(),
        };
        Ok(service
            .cwd
            .as_ref()
            .map(|cwd| base.join(cwd))
            .unwrap_or(base))
    }
}

enum StartOne {
    Started(Box<RuntimeEntry>),
    AlreadyRunning,
}

fn ensure_matching(entry: &RuntimeEntry, name: &str) -> Result<()> {
    match inspect_identity(entry) {
        IdentityStatus::Matching => Ok(()),
        IdentityStatus::Missing => Err(anyhow!("service '{name}' process exited")),
        IdentityStatus::Mismatch(reason) => {
            Err(anyhow!("service '{name}' identity mismatch: {reason}"))
        }
    }
}

fn readiness_label(mode: ReadyMode) -> &'static str {
    match mode {
        ReadyMode::Started => "started",
        ReadyMode::Listening => "listening",
        ReadyMode::Healthy => "healthy",
    }
}

fn stable_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn config_digest<T: Serialize>(value: &T) -> Result<String> {
    let mut value = serde_json::to_value(value)?;
    canonicalize_json(&mut value);
    Ok(stable_digest(&serde_json::to_vec(&value)?))
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries = std::mem::take(object).into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in entries {
                canonicalize_json(&mut value);
                object.insert(key, value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::{ServiceConfig, TemplateConfig};
    use crate::runtime::process::DetachedStopOutcome;

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hum-detached-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn loaded(root: &Path, command: &str) -> Loaded {
        Loaded {
            config: Config {
                version: 2,
                project: Some("demo".to_string()),
                services: HashMap::from([(
                    "server".to_string(),
                    ServiceConfig {
                        command: Some(command.to_string()),
                        ..ServiceConfig::default()
                    },
                )]),
                templates: HashMap::from([(
                    "all".to_string(),
                    TemplateConfig {
                        services: vec!["server".to_string()],
                    },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root.to_path_buf(),
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detached_service_survives_runtime_drop_and_is_rediscovered() {
        let root = temp_root("survives");
        let state = root.join("state");
        let runtime = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "echo detached-ok; while :; do sleep 1; done"),
            HashMap::new(),
            state.clone(),
        )
        .unwrap();
        let report = runtime.start_template("all").await.unwrap();
        assert_eq!(report.started, ["server"]);
        let entry = runtime.registry().load("server").unwrap().unwrap();
        assert_eq!(inspect_identity(&entry), IdentityStatus::Matching);
        assert_eq!(unsafe { libc::getsid(entry.pid as i32) }, entry.pid as i32);
        drop(runtime);

        tokio::time::sleep(Duration::from_millis(100)).await;
        let registry = RuntimeRegistry::at(state, "demo").unwrap();
        let rediscovered = registry.load("server").unwrap().unwrap();
        assert_eq!(inspect_identity(&rediscovered), IdentityStatus::Matching);
        let stdout = fs::read_to_string(&rediscovered.stdout_log).unwrap();
        assert!(stdout.contains("detached-ok"));

        assert!(matches!(
            process::stop_detached(&rediscovered, Duration::from_secs(2))
                .await
                .unwrap(),
            DetachedStopOutcome::Stopped | DetachedStopOutcome::AlreadyMissing
        ));
        registry.remove("server").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_starts_are_serialized_and_idempotent() {
        let root = temp_root("concurrent");
        let state = root.join("state");
        let first = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "while :; do sleep 1; done"),
            HashMap::new(),
            state.clone(),
        )
        .unwrap();
        let second = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "while :; do sleep 1; done"),
            HashMap::new(),
            state,
        )
        .unwrap();
        let (one, two) = tokio::join!(first.start_template("all"), second.start_template("all"));
        let one = one.unwrap();
        let two = two.unwrap();
        assert_eq!(one.started.len() + two.started.len(), 1);
        assert_eq!(one.already_running.len() + two.already_running.len(), 1);

        let entry = first.registry().load("server").unwrap().unwrap();
        process::stop_detached(&entry, Duration::from_secs(2))
            .await
            .unwrap();
        first.registry().remove("server").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_mismatch_never_signals_reused_pid() {
        let pid = std::process::id();
        let actual_start = process_start_time(pid).unwrap();
        let pgid = unsafe { libc::getpgid(pid as i32) };
        let entry = RuntimeEntry::new(
            "demo".to_string(),
            "server".to_string(),
            pid,
            pgid,
            actual_start.wrapping_add(1),
            "definitely-not-this-process-token".to_string(),
            PathBuf::from("/tmp/identity-does-not-exist.lock"),
            "command".to_string(),
            "config".to_string(),
            None,
            std::env::temp_dir(),
            PathBuf::from("stdout"),
            PathBuf::from("stderr"),
        );
        let outcome = process::stop_detached(&entry, Duration::from_millis(10))
            .await
            .unwrap();
        assert!(matches!(outcome, DetachedStopOutcome::IdentityMismatch(_)));
        assert!(process_start_time(pid).is_ok());
    }

    #[test]
    fn service_config_digest_is_canonical_across_map_insertion_order() {
        let mut first = ServiceConfig::default();
        first.env.insert("ALPHA".to_string(), "one".to_string());
        first.env.insert("BETA".to_string(), "two".to_string());
        let mut second = ServiceConfig::default();
        second.env.insert("BETA".to_string(), "two".to_string());
        second.env.insert("ALPHA".to_string(), "one".to_string());

        assert_eq!(
            config_digest(&first).unwrap(),
            config_digest(&second).unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn descendants_remain_owned_after_session_leader_exits() {
        let root = temp_root("leader-exit");
        let state = root.join("state");
        let runtime = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "sleep 30 &"),
            HashMap::new(),
            state,
        )
        .unwrap();
        runtime.start_template("all").await.unwrap();
        let entry = runtime.registry().load("server").unwrap().unwrap();

        for _ in 0..20 {
            if unsafe { libc::getpgid(entry.pid as i32) } < 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(unsafe { libc::getpgid(entry.pid as i32) } < 0);
        assert_eq!(inspect_identity(&entry), IdentityStatus::Matching);
        assert_eq!(
            process::stop_detached(&entry, Duration::from_secs(1))
                .await
                .unwrap(),
            DetachedStopOutcome::Stopped
        );
        runtime.registry().remove("server").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crashed_entry_is_stale_and_next_start_replaces_it() {
        let root = temp_root("crash");
        let state = root.join("state");
        let runtime = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "sleep 0.1; exit 7"),
            HashMap::new(),
            state,
        )
        .unwrap();
        runtime.start_template("all").await.unwrap();
        let first = runtime.registry().load("server").unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(inspect_identity(&first), IdentityStatus::Missing);

        let report = runtime.start_template("all").await.unwrap();
        assert_eq!(report.started, ["server"]);
        let second = runtime.registry().load("server").unwrap().unwrap();
        assert_ne!(first.started_at, second.started_at);
        tokio::time::sleep(Duration::from_millis(200)).await;
        runtime.registry().remove("server").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_start_rolls_back_only_new_processes() {
        let root = temp_root("rollback");
        let state = root.join("state");
        let mut loaded = loaded(&root, "while :; do sleep 1; done");
        loaded.config.services.insert(
            "broken".to_string(),
            ServiceConfig {
                cwd: Some("missing-directory".into()),
                command: Some("true".to_string()),
                ..ServiceConfig::default()
            },
        );
        loaded.config.templates.get_mut("all").unwrap().services =
            vec!["server".to_string(), "broken".to_string()];
        let runtime =
            DetachedRuntime::with_state_root("demo".to_string(), loaded, HashMap::new(), state)
                .unwrap();

        assert!(runtime.start_template("all").await.is_err());
        assert!(runtime.registry().load("server").unwrap().is_none());
        assert!(runtime.registry().load("broken").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
