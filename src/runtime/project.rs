use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::config::{Config, Loaded, RuntimeConfig};
use crate::core::graph;

use super::adapter::RuntimeAdapter;
use super::compose::ComposeRuntime;
use super::detached::{
    DetachedRuntime, DetachedServiceStatus, RestartReport, StartReport, StopReport,
};
use super::registry::RuntimeRegistry;
use super::task::TaskRunner;

/// Coordinates one dependency graph across heterogeneous runtime adapters.
pub struct ProjectRuntime {
    project: String,
    process: Arc<DetachedRuntime>,
    adapters: Vec<Arc<dyn RuntimeAdapter>>,
    tasks: TaskRunner,
}

impl ProjectRuntime {
    pub fn new(
        project: String,
        loaded: Loaded,
        env_overrides: HashMap<String, String>,
    ) -> Result<Self> {
        let config = loaded.config.clone();
        let root_dir = loaded.root_dir.clone();
        let process = Arc::new(DetachedRuntime::new(
            project.clone(),
            loaded,
            env_overrides.clone(),
        )?);
        let tasks = TaskRunner::new(config.clone(), root_dir.clone(), env_overrides.clone());
        let mut adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![process.clone()];
        if config.version >= 3 {
            let mut runtime_names = config.runtimes.keys().cloned().collect::<Vec<_>>();
            runtime_names.sort();
            for name in runtime_names {
                if matches!(config.runtimes[&name], RuntimeConfig::Compose { .. }) {
                    adapters.push(Arc::new(ComposeRuntime::new(
                        name,
                        project.clone(),
                        config.clone(),
                        root_dir.clone(),
                        env_overrides.clone(),
                    )?));
                }
            }
        }
        Ok(Self {
            project,
            process,
            adapters,
            tasks,
        })
    }

    pub fn config(&self) -> &Config {
        self.process.config()
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn registry(&self) -> &RuntimeRegistry {
        self.process.registry()
    }

    pub fn process_runtime(&self) -> &DetachedRuntime {
        &self.process
    }

    #[cfg(test)]
    pub(crate) fn with_state_root(
        project: String,
        loaded: Loaded,
        env_overrides: HashMap<String, String>,
        state_root: PathBuf,
    ) -> Result<Self> {
        let config = loaded.config.clone();
        let root_dir = loaded.root_dir.clone();
        let tasks = TaskRunner::new(config, root_dir, env_overrides.clone());
        let process = Arc::new(DetachedRuntime::with_state_root(
            project.clone(),
            loaded,
            env_overrides,
            state_root,
        )?);
        Ok(Self {
            project,
            adapters: vec![process.clone()],
            process,
            tasks,
        })
    }

    pub fn has_compose_runtime(&self) -> bool {
        self.config()
            .runtimes
            .values()
            .any(|runtime| matches!(runtime, RuntimeConfig::Compose { .. }))
    }

    pub async fn start_services(&self, services: &[String]) -> Result<StartReport> {
        let order = graph::resolve_start_order(self.config(), services)?;
        self.start_ordered(&order).await
    }

    pub async fn start_selection(&self, order: &[String]) -> Result<StartReport> {
        self.start_ordered(order).await
    }

    pub async fn sync_selection_environment(
        &self,
        order: &[String],
    ) -> Result<crate::config::environment::EnvironmentSyncReport> {
        self.sync_ordered_environment(order).await
    }

    pub async fn stop_template(&self, template: &str, grace: Duration) -> Result<StopReport> {
        let order = graph::stop_order(&graph::services_for_template(self.config(), template)?);
        self.stop_ordered(&order, grace).await
    }

    pub async fn stop_services(&self, services: &[String], grace: Duration) -> Result<StopReport> {
        self.stop_ordered(&graph::stop_order(services), grace).await
    }

    pub async fn restart_template(&self, template: &str, grace: Duration) -> Result<RestartReport> {
        let start_order = graph::services_for_template(self.config(), template)?;
        let stop_order = graph::stop_order(&start_order);
        self.restart_ordered(&stop_order, &start_order, grace).await
    }

    pub async fn restart_services(
        &self,
        services: &[String],
        grace: Duration,
    ) -> Result<RestartReport> {
        let start_order = graph::resolve_start_order(self.config(), services)?;
        let requested = services.iter().collect::<std::collections::HashSet<_>>();
        let stop_order = graph::stop_order(
            &start_order
                .iter()
                .filter(|service| requested.contains(service))
                .cloned()
                .collect::<Vec<_>>(),
        );
        self.restart_ordered(&stop_order, &start_order, grace).await
    }

    pub async fn status_template(&self, template: &str) -> Result<Vec<DetachedServiceStatus>> {
        let order = graph::services_for_template(self.config(), template)?;
        self.collect_statuses(&order, true).await
    }

    pub async fn monitor_template(&self, template: &str) -> Result<Vec<DetachedServiceStatus>> {
        let order = graph::services_for_template(self.config(), template)?;
        self.collect_statuses(&order, false).await
    }

    pub async fn check_service_health(
        &self,
        name: &str,
    ) -> Result<(crate::core::state::HealthState, String, u64)> {
        let service = self
            .config()
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let Some(check) = service.healthcheck.as_ref() else {
            return Ok((
                crate::core::state::HealthState::Unchecked,
                "not configured".to_string(),
                0,
            ));
        };
        let started = std::time::Instant::now();
        let result = super::health::check_once(check).await;
        let duration = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        Ok(match result {
            Ok(()) => (
                crate::core::state::HealthState::Healthy,
                "ok".to_string(),
                duration,
            ),
            Err(detail) => (crate::core::state::HealthState::Unhealthy, detail, duration),
        })
    }

    pub fn log_files(&self, service: &str) -> Result<Option<(PathBuf, PathBuf)>> {
        self.adapter_for(service)?.log_files(service)
    }

    pub fn log_paths(&self, service: &str) -> Result<(PathBuf, PathBuf)> {
        self.log_files(service)?.ok_or_else(|| {
            anyhow!("service '{service}' uses runtime-native logs without persistent files")
        })
    }

    pub async fn stream_external_logs(
        &self,
        services: &[String],
        lines: usize,
        follow: bool,
    ) -> Result<()> {
        let futures = self.adapters.iter().filter_map(|adapter| {
            let owned = services
                .iter()
                .filter(|service| {
                    adapter.owns_service(service) && adapter.log_files(service).ok() == Some(None)
                })
                .cloned()
                .collect::<Vec<_>>();
            (!owned.is_empty())
                .then_some(async move { adapter.stream_logs(&owned, lines, follow).await })
        });
        futures_util::future::try_join_all(futures).await?;
        Ok(())
    }

    pub async fn capture_external_logs(&self, service: &str, lines: usize) -> Result<Vec<String>> {
        let adapter = self.adapter_for(service)?;
        if adapter.log_files(service)?.is_some() {
            return Err(anyhow!(
                "service '{service}' uses persistent logs instead of runtime-native logs"
            ));
        }
        let services = vec![service.to_string()];
        adapter.capture_logs(&services, lines).await
    }

    pub async fn reset_all(&self, grace: Duration) -> Result<StopReport> {
        let all = self.config().services.keys().cloned().collect::<Vec<_>>();
        let order = graph::stop_order(&graph::resolve_start_order(self.config(), &all)?);
        let stop = self.stop_ordered(&order, grace).await?;
        if !stop.succeeded() {
            return Ok(stop);
        }
        for adapter in &self.adapters {
            adapter.reset().await?;
        }
        Ok(stop)
    }

    fn adapter_for(&self, service: &str) -> Result<&Arc<dyn RuntimeAdapter>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.owns_service(service))
            .ok_or_else(|| anyhow!("no runtime adapter owns service '{service}'"))
    }

    async fn collect_statuses(
        &self,
        order: &[String],
        check_health: bool,
    ) -> Result<Vec<DetachedServiceStatus>> {
        let service_order = order
            .iter()
            .filter(|name| self.config().services.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        let mut by_name = HashMap::new();
        for adapter in &self.adapters {
            let owned = service_order
                .iter()
                .filter(|service| adapter.owns_service(service))
                .cloned()
                .collect::<Vec<_>>();
            if owned.is_empty() {
                continue;
            }
            let statuses = if check_health {
                adapter.status_services(&owned).await?
            } else {
                adapter.monitor_services(&owned).await?
            };
            by_name.extend(
                statuses
                    .into_iter()
                    .map(|status| (status.name.clone(), status)),
            );
        }
        service_order
            .iter()
            .map(|name| {
                by_name
                    .remove(name)
                    .ok_or_else(|| anyhow!("runtime returned no status for service '{name}'"))
            })
            .collect()
    }

    async fn start_ordered(&self, order: &[String]) -> Result<StartReport> {
        let mut combined = StartReport::default();
        let mut started = Vec::new();
        for service in order {
            if self.config().tasks.contains_key(service) {
                if let Err(error) = self.tasks.run(service).await {
                    self.rollback(&started).await;
                    return Err(error);
                }
                continue;
            }
            let adapter = self.adapter_for(service)?;
            match adapter.start_services(std::slice::from_ref(service)).await {
                Ok(report) => {
                    let newly_started = report.started.clone();
                    combined.started.extend(report.started);
                    combined.already_running.extend(report.already_running);
                    combined.reconciled.extend(report.reconciled);
                    if let Err(error) = adapter.wait_ready(service).await {
                        self.rollback(&started).await;
                        return Err(
                            error.context(format!("service '{service}' did not become ready"))
                        );
                    }
                    if !newly_started.is_empty() {
                        started.push(service.clone());
                    }
                }
                Err(error) => {
                    self.rollback(&started).await;
                    return Err(error.context(format!("failed to start service '{service}'")));
                }
            }
        }
        Ok(combined)
    }

    async fn sync_ordered_environment(
        &self,
        order: &[String],
    ) -> Result<crate::config::environment::EnvironmentSyncReport> {
        let mut sources = Vec::new();
        for name in order {
            if let Some(service) = self.config().services.get(name) {
                sources.extend(service.env_from.iter().cloned());
            } else if let Some(task) = self.config().tasks.get(name) {
                sources.extend(task.env_from.iter().cloned());
            }
        }
        crate::config::environment::sync_environment_sources(
            self.config(),
            &sources,
            self.process.root_dir(),
        )
        .await
    }

    async fn rollback(&self, started: &[String]) {
        for service in started.iter().rev() {
            if let Ok(adapter) = self.adapter_for(service) {
                let _ = adapter
                    .stop_services(std::slice::from_ref(service), Duration::from_secs(10))
                    .await;
            }
        }
    }

    async fn stop_ordered(&self, order: &[String], grace: Duration) -> Result<StopReport> {
        let mut combined = StopReport::default();
        let services = order
            .iter()
            .filter(|name| self.config().services.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        for (index, service) in services.iter().enumerate() {
            let adapter = self.adapter_for(service)?;
            let report = adapter
                .stop_services(std::slice::from_ref(service), grace)
                .await?;
            combined.stopped.extend(report.stopped);
            combined.already_stopped.extend(report.already_stopped);
            combined.stale_removed.extend(report.stale_removed);
            combined.blocked.extend(report.blocked);
            combined.failures.extend(report.failures);
            if !combined.failures.is_empty() {
                combined.blocked.extend_from_slice(&services[index + 1..]);
                break;
            }
        }
        Ok(combined)
    }

    async fn restart_ordered(
        &self,
        stop_order: &[String],
        start_order: &[String],
        grace: Duration,
    ) -> Result<RestartReport> {
        let stop = self.stop_ordered(stop_order, grace).await?;
        if !stop.succeeded() {
            return Ok(RestartReport { stop, start: None });
        }
        let start = self
            .start_ordered(start_order)
            .await
            .with_context(|| format!("project '{}' restart failed during start", self.project))?;
        Ok(RestartReport {
            stop,
            start: Some(start),
        })
    }
}
