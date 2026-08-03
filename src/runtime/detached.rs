use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{Config, HealthcheckConfig, Loaded, ReadyMode};
use crate::core::graph;
use crate::core::state::{HealthState, PortState, PresentationState, ProcessState};

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

#[derive(Debug, Default)]
pub struct StopReport {
    pub stopped: Vec<String>,
    pub already_stopped: Vec<String>,
    pub stale_removed: Vec<String>,
    pub blocked: Vec<String>,
    pub failures: Vec<StopFailure>,
}

impl StopReport {
    pub fn succeeded(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug)]
pub struct StopFailure {
    pub service: String,
    pub detail: String,
}

#[derive(Debug)]
pub struct RestartReport {
    pub stop: StopReport,
    pub start: Option<StartReport>,
}

#[derive(Debug, Clone)]
pub struct DetachedServiceStatus {
    pub name: String,
    pub process: ProcessState,
    pub port: PortState,
    pub health: HealthState,
    pub configured_port: Option<u16>,
    pub pid: Option<u32>,
    pub detail: Option<String>,
    pub health_detail: Option<String>,
    pub health_duration_ms: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
}

impl DetachedServiceStatus {
    pub fn presentation(&self) -> PresentationState {
        match self.process {
            ProcessState::Starting => PresentationState::Starting,
            ProcessState::Stopping => PresentationState::Stopping,
            ProcessState::Exited => PresentationState::Exited,
            ProcessState::Missing if self.detail.is_some() => PresentationState::Blocked,
            ProcessState::Missing => PresentationState::Missing,
            ProcessState::Running if self.health == HealthState::Unhealthy => {
                PresentationState::Degraded
            }
            ProcessState::Running
                if self.health == HealthState::Healthy
                    || (self.health == HealthState::Unchecked
                        && self.port == PortState::Listening) =>
            {
                PresentationState::Ready
            }
            ProcessState::Running => PresentationState::Running,
        }
    }
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
    pub(crate) fn with_state_root(
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

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn root_dir(&self) -> &std::path::Path {
        &self.root_dir
    }

    pub fn env_overrides(&self) -> &HashMap<String, String> {
        &self.env_overrides
    }

    pub async fn start_template(&self, template: &str) -> Result<StartReport> {
        let order = graph::services_for_template(&self.config, template)?;
        self.start_ordered(&order).await
    }

    pub async fn start_services(&self, services: &[String]) -> Result<StartReport> {
        let order = graph::resolve_start_order(&self.config, services)?;
        self.start_ordered(&order).await
    }

    pub async fn status_template(&self, template: &str) -> Result<Vec<DetachedServiceStatus>> {
        let order = graph::services_for_template(&self.config, template)?;
        self.status_ordered(&order, true).await
    }

    pub async fn monitor_template(&self, template: &str) -> Result<Vec<DetachedServiceStatus>> {
        let order = graph::services_for_template(&self.config, template)?;
        self.status_ordered(&order, false).await
    }

    pub async fn check_service_health(&self, name: &str) -> Result<(HealthState, String, u64)> {
        let service = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let Some(check) = service.healthcheck.as_ref() else {
            return Ok((HealthState::Unchecked, "not configured".to_string(), 0));
        };
        let started = std::time::Instant::now();
        let result = health::check_once(check).await;
        let duration = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        Ok(match result {
            Ok(()) => (HealthState::Healthy, "ok".to_string(), duration),
            Err(detail) => (HealthState::Unhealthy, detail, duration),
        })
    }

    pub async fn stop_template(&self, template: &str, grace: Duration) -> Result<StopReport> {
        let order = graph::stop_order(&graph::services_for_template(&self.config, template)?);
        self.stop_ordered(&order, grace).await
    }

    pub async fn stop_services(&self, services: &[String], grace: Duration) -> Result<StopReport> {
        let requested = services.iter().cloned().collect::<HashSet<_>>();
        let start_order = graph::resolve_start_order(&self.config, services)?;
        let order = graph::stop_order(
            &start_order
                .into_iter()
                .filter(|name| requested.contains(name))
                .collect::<Vec<_>>(),
        );
        self.stop_ordered(&order, grace).await
    }

    pub async fn restart_template(&self, template: &str, grace: Duration) -> Result<RestartReport> {
        let start_order = graph::services_for_template(&self.config, template)?;
        let stop_order = graph::stop_order(&start_order);
        self.restart_ordered(&stop_order, &start_order, grace).await
    }

    pub async fn restart_services(
        &self,
        services: &[String],
        grace: Duration,
    ) -> Result<RestartReport> {
        let start_order = graph::resolve_start_order(&self.config, services)?;
        let requested = services.iter().cloned().collect::<HashSet<_>>();
        let stop_order = graph::stop_order(
            &start_order
                .iter()
                .filter(|name| requested.contains(*name))
                .cloned()
                .collect::<Vec<_>>(),
        );
        self.restart_ordered(&stop_order, &start_order, grace).await
    }

    pub fn log_paths(&self, service: &str) -> Result<(PathBuf, PathBuf)> {
        if !self.config.services.contains_key(service) {
            anyhow::bail!("unknown service '{service}'");
        }
        Ok(self.registry.log_paths(service))
    }

    async fn start_ordered(&self, order: &[String]) -> Result<StartReport> {
        let registry = self.registry.clone();
        let _lock = tokio::task::spawn_blocking(move || registry.lock())
            .await
            .context("project lock task failed")??;
        self.start_ordered_locked(order).await
    }

    async fn start_ordered_locked(&self, order: &[String]) -> Result<StartReport> {
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

    async fn status_ordered(
        &self,
        order: &[String],
        check_health: bool,
    ) -> Result<Vec<DetachedServiceStatus>> {
        let registry = self.registry.clone();
        let project_lock = tokio::task::spawn_blocking(move || registry.lock())
            .await
            .context("project lock task failed")??;
        let mut snapshots = Vec::with_capacity(order.len());
        for name in order {
            let snapshot = match self
                .registry
                .load(name)
                .with_context(|| format!("service '{name}' runtime entry could not be read"))?
            {
                None => StatusSnapshot {
                    entry: None,
                    process: ProcessState::Missing,
                    detail: None,
                },
                Some(entry) => match inspect_identity(&entry) {
                    IdentityStatus::Matching => StatusSnapshot {
                        entry: Some(entry),
                        process: ProcessState::Running,
                        detail: None,
                    },
                    IdentityStatus::Missing => {
                        process::wait_log_sink_exit(&entry).await.with_context(|| {
                            format!("service '{name}' log sink is still draining")
                        })?;
                        self.registry.remove(name).with_context(|| {
                            format!("service '{name}' stale runtime entry could not be removed")
                        })?;
                        StatusSnapshot {
                            entry: None,
                            process: ProcessState::Missing,
                            detail: Some("stale runtime entry removed".to_string()),
                        }
                    }
                    IdentityStatus::Mismatch(reason) => StatusSnapshot {
                        entry: None,
                        process: ProcessState::Missing,
                        detail: Some(format!(
                            "runtime identity mismatch: {reason}; verify the process before removing its registry entry"
                        )),
                    },
                },
            };
            snapshots.push((name.clone(), snapshot));
        }
        drop(project_lock);

        let mut statuses = Vec::with_capacity(snapshots.len());
        for (name, snapshot) in snapshots {
            let service = self
                .config
                .services
                .get(&name)
                .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
            let port = self.observe_port(service.port, snapshot.entry.as_ref(), check_health);
            let health = match service
                .healthcheck
                .as_ref()
                .filter(|_| check_health && snapshot.process.is_running())
            {
                Some(check) if health::check_once(check).await.is_ok() => HealthState::Healthy,
                Some(_) => HealthState::Unhealthy,
                None => HealthState::Unchecked,
            };
            statuses.push(DetachedServiceStatus {
                name,
                process: snapshot.process,
                port,
                health,
                configured_port: service.port,
                pid: snapshot.entry.as_ref().map(|entry| entry.pid),
                detail: snapshot.detail,
                health_detail: None,
                health_duration_ms: None,
                started_at: snapshot.entry.as_ref().map(|entry| entry.started_at),
            });
        }
        Ok(statuses)
    }

    async fn stop_ordered(&self, order: &[String], grace: Duration) -> Result<StopReport> {
        let registry = self.registry.clone();
        let _lock = tokio::task::spawn_blocking(move || registry.lock())
            .await
            .context("project lock task failed")??;
        self.stop_ordered_locked(order, grace).await
    }

    async fn stop_ordered_locked(&self, order: &[String], grace: Duration) -> Result<StopReport> {
        let mut report = StopReport::default();
        for (index, name) in order.iter().enumerate() {
            let entry = match self.registry.load(name) {
                Ok(entry) => entry,
                Err(error) => {
                    report.failures.push(StopFailure {
                        service: name.clone(),
                        detail: format!(
                            "project '{}' service '{name}' runtime entry could not be read: {error:#}; validate or repair its registry entry before retrying",
                            self.project
                        ),
                    });
                    report.blocked.extend_from_slice(&order[index + 1..]);
                    break;
                }
            };
            let Some(entry) = entry else {
                report.already_stopped.push(name.clone());
                continue;
            };
            match inspect_identity(&entry) {
                IdentityStatus::Missing => match self.registry.remove(name) {
                    Ok(()) => report.stale_removed.push(name.clone()),
                    Err(error) => {
                        report.failures.push(StopFailure {
                                service: name.clone(),
                                detail: format!(
                                    "project '{}' service '{name}' stopped but stale registry cleanup failed: {error:#}; repair the runtime directory before retrying",
                                    self.project
                                ),
                            });
                        report.blocked.extend_from_slice(&order[index + 1..]);
                        break;
                    }
                },
                IdentityStatus::Mismatch(reason) => {
                    report.failures.push(StopFailure {
                        service: name.clone(),
                        detail: format!(
                            "project '{}' service '{name}' identity mismatch: {reason}; verify the PID/PGID and retry only after correcting the runtime entry",
                            self.project
                        ),
                    });
                    report.blocked.extend_from_slice(&order[index + 1..]);
                    break;
                }
                IdentityStatus::Matching => match process::stop_detached(&entry, grace).await {
                    Ok(
                        outcome @ (DetachedStopOutcome::Stopped
                        | DetachedStopOutcome::AlreadyMissing),
                    ) => match self.registry.remove(name) {
                        Ok(()) => match outcome {
                            DetachedStopOutcome::Stopped => report.stopped.push(name.clone()),
                            DetachedStopOutcome::AlreadyMissing => {
                                report.stale_removed.push(name.clone())
                            }
                            DetachedStopOutcome::IdentityMismatch(_) => unreachable!(),
                        },
                        Err(error) => {
                            report.failures.push(StopFailure {
                                    service: name.clone(),
                                    detail: format!(
                                        "project '{}' service '{name}' exited but registry cleanup failed: {error:#}; repair the runtime directory before retrying",
                                        self.project
                                    ),
                                });
                            report.blocked.extend_from_slice(&order[index + 1..]);
                            break;
                        }
                    },
                    Ok(DetachedStopOutcome::IdentityMismatch(reason)) => {
                        report.failures.push(StopFailure {
                            service: name.clone(),
                            detail: format!(
                                "project '{}' service '{name}' changed identity while stopping: {reason}; inspect the current process before retrying",
                                self.project
                            ),
                        });
                        report.blocked.extend_from_slice(&order[index + 1..]);
                        break;
                    }
                    Err(error) => {
                        report.failures.push(StopFailure {
                            service: name.clone(),
                            detail: format!(
                                "project '{}' service '{name}' could not be stopped: {error:#}; inspect its process group and retry",
                                self.project
                            ),
                        });
                        report.blocked.extend_from_slice(&order[index + 1..]);
                        break;
                    }
                },
            }
        }
        Ok(report)
    }

    async fn restart_ordered(
        &self,
        stop_order: &[String],
        start_order: &[String],
        grace: Duration,
    ) -> Result<RestartReport> {
        let registry = self.registry.clone();
        let _lock = tokio::task::spawn_blocking(move || registry.lock())
            .await
            .context("project lock task failed")??;
        let stop = self.stop_ordered_locked(stop_order, grace).await?;
        if !stop.succeeded() {
            return Ok(RestartReport { stop, start: None });
        }
        let start = self.start_ordered_locked(start_order).await?;
        Ok(RestartReport {
            stop,
            start: Some(start),
        })
    }

    fn observe_port(
        &self,
        port: Option<u16>,
        entry: Option<&RuntimeEntry>,
        diagnose_owner: bool,
    ) -> PortState {
        let Some(port) = port else {
            return PortState::Unknown;
        };
        match portcheck::probe_port(port) {
            portcheck::PortProbe::Closed => PortState::Closed,
            portcheck::PortProbe::Unknown => PortState::Unknown,
            portcheck::PortProbe::Listening => match entry {
                Some(_) if !diagnose_owner => PortState::ListeningUnverified,
                Some(entry) => {
                    let occupant = portcheck::identify_occupant(port);
                    if occupant
                        .pid
                        .is_some_and(|pid| portcheck::belongs_to_process_group(pid, entry.pgid))
                    {
                        PortState::Listening
                    } else {
                        PortState::OccupiedByOther {
                            pid: occupant.pid,
                            process_name: occupant.process_name,
                        }
                    }
                }
                None => PortState::OccupiedByOther {
                    pid: None,
                    process_name: None,
                },
            },
        }
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
        let config_hash = config_digest(&(service, &self.config.logs))?;

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
                    process::wait_log_sink_exit(&entry).await.with_context(|| {
                        format!("previous log sink for service '{name}' is still draining")
                    })?;
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
            super::logs::LogPolicy::from(&self.config.logs),
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
            process.log_sink_pid,
            process.log_sink_start_time,
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

struct StatusSnapshot {
    entry: Option<RuntimeEntry>,
    process: ProcessState,
    detail: Option<String>,
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

    #[test]
    fn unverified_listener_never_marks_a_service_ready() {
        let status = DetachedServiceStatus {
            name: "server".to_string(),
            process: ProcessState::Running,
            port: PortState::ListeningUnverified,
            health: HealthState::Unchecked,
            configured_port: Some(8080),
            pid: Some(123),
            detail: None,
            health_detail: None,
            health_duration_ms: None,
            started_at: None,
        };

        assert_eq!(status.presentation(), PresentationState::Running);
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
    async fn monitor_reconciles_external_crash_and_restart_across_instances() {
        let root = temp_root("monitor-reconcile");
        let state = root.join("state");
        let first = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "while :; do sleep 1; done"),
            HashMap::new(),
            state.clone(),
        )
        .unwrap();
        first.start_template("all").await.unwrap();
        let entry = first.registry().load("server").unwrap().unwrap();
        drop(first);

        let monitor = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "while :; do sleep 1; done"),
            HashMap::new(),
            state.clone(),
        )
        .unwrap();
        let running = monitor.monitor_template("all").await.unwrap();
        assert_eq!(running[0].process, ProcessState::Running);
        assert_eq!(running[0].pid, Some(entry.pid));

        assert_eq!(unsafe { libc::kill(-entry.pgid, libc::SIGKILL) }, 0);
        for _ in 0..40 {
            if inspect_identity(&entry) == IdentityStatus::Missing {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let crashed = monitor.monitor_template("all").await.unwrap();
        assert_eq!(crashed[0].process, ProcessState::Missing);
        assert!(crashed[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("stale runtime entry")));

        let external_controller = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "while :; do sleep 1; done"),
            HashMap::new(),
            state,
        )
        .unwrap();
        external_controller.start_template("all").await.unwrap();
        let restarted = monitor.monitor_template("all").await.unwrap();
        assert_eq!(restarted[0].process, ProcessState::Running);
        let replacement = monitor.registry().load("server").unwrap().unwrap();
        process::stop_detached(&replacement, Duration::from_secs(2))
            .await
            .unwrap();
        monitor.registry().remove("server").unwrap();
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
            None,
            None,
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
        for _ in 0..40 {
            if inspect_identity(&first) == IdentityStatus::Missing {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn template_stop_uses_reverse_dependency_order() {
        let root = temp_root("stop-order");
        let state = root.join("state");
        let mut loaded = loaded(&root, "while :; do sleep 1; done");
        loaded.config.services.clear();
        loaded.config.services.insert(
            "database".to_string(),
            ServiceConfig {
                command: Some("while :; do sleep 1; done".to_string()),
                ..ServiceConfig::default()
            },
        );
        loaded.config.services.insert(
            "api".to_string(),
            ServiceConfig {
                command: Some("while :; do sleep 1; done".to_string()),
                depends_on: vec!["database".to_string()],
                ..ServiceConfig::default()
            },
        );
        loaded.config.templates.get_mut("all").unwrap().services = vec!["api".to_string()];
        let runtime =
            DetachedRuntime::with_state_root("demo".to_string(), loaded, HashMap::new(), state)
                .unwrap();

        runtime.start_template("all").await.unwrap();
        let report = runtime
            .stop_template("all", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(report.succeeded());
        assert_eq!(report.stopped, ["api", "database"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_dependent_stop_does_not_stop_its_dependency() {
        let root = temp_root("stop-blocks-dependency");
        let state = root.join("state");
        let mut loaded = loaded(&root, "while :; do sleep 1; done");
        loaded.config.services.clear();
        for (name, dependencies) in [
            ("database", Vec::new()),
            ("api", vec!["database".to_string()]),
        ] {
            loaded.config.services.insert(
                name.to_string(),
                ServiceConfig {
                    command: Some("while :; do sleep 1; done".to_string()),
                    depends_on: dependencies,
                    ..ServiceConfig::default()
                },
            );
        }
        loaded.config.templates.get_mut("all").unwrap().services = vec!["api".to_string()];
        let runtime =
            DetachedRuntime::with_state_root("demo".to_string(), loaded, HashMap::new(), state)
                .unwrap();
        runtime.start_template("all").await.unwrap();
        let api = runtime.registry().load("api").unwrap().unwrap();
        let database = runtime.registry().load("database").unwrap().unwrap();
        fs::remove_file(&api.identity_file).unwrap();

        let report = runtime
            .stop_template("all", Duration::from_millis(10))
            .await
            .unwrap();
        assert!(!report.succeeded());
        assert_eq!(report.failures[0].service, "api");
        assert_eq!(report.blocked, ["database"]);
        assert_eq!(inspect_identity(&database), IdentityStatus::Matching);

        process::abort_unregistered(process::DetachedProcess {
            pid: api.pid,
            pgid: api.pgid,
            log_sink_pid: api.log_sink_pid,
            log_sink_start_time: api.log_sink_start_time,
        })
        .unwrap();
        process::abort_unregistered(process::DetachedProcess {
            pid: database.pid,
            pgid: database.pgid,
            log_sink_pid: database.log_sink_pid,
            log_sink_start_time: database.log_sink_start_time,
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        runtime.registry().remove("api").unwrap();
        runtime.registry().remove("database").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_stop_confirms_process_group_exit_before_registry_removal() {
        let root = temp_root("kill-confirmation");
        let state = root.join("state");
        let runtime = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded(&root, "trap '' TERM; while :; do sleep 1; done"),
            HashMap::new(),
            state,
        )
        .unwrap();
        runtime.start_template("all").await.unwrap();
        let entry = runtime.registry().load("server").unwrap().unwrap();

        let report = runtime
            .stop_template("all", Duration::from_millis(1))
            .await
            .unwrap();
        assert!(report.succeeded());
        let system = sysinfo::System::new_all();
        assert!(!system.processes().values().any(|process| {
            (unsafe { libc::getpgid(process.pid().as_u32() as i32) }) == entry.pgid
        }));
        assert!(runtime.registry().load("server").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
