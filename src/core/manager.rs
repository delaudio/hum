use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::config::{Config, HealthcheckConfig, Loaded, ReadyMode};
use crate::runtime::health;
use crate::runtime::logs::{LogBuffer, LogLine, Stream as LogStream};
use crate::runtime::portcheck;
use crate::runtime::process::RunningProcess;

use super::graph;
use super::state::ServiceStatus;

const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything `hum` tracks about one service while it's part of the running
/// session.
pub struct ServiceRuntime {
    pub status: Mutex<ServiceStatus>,
    pub process: Mutex<Option<Arc<RunningProcess>>>,
    pub logs: Arc<LogBuffer>,
    pub started_at: Mutex<Option<Instant>>,
    pub last_health: Mutex<Option<HealthResult>>,
    pub blocked_reason: Mutex<Option<String>>,
    health_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stop_health: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HealthResult {
    pub ok: bool,
    pub detail: String,
    pub checked_at: Instant,
}

impl ServiceRuntime {
    fn new() -> Self {
        ServiceRuntime {
            status: Mutex::new(ServiceStatus::Stopped),
            process: Mutex::new(None),
            logs: LogBuffer::new(crate::runtime::logs::DEFAULT_CAPACITY),
            started_at: Mutex::new(None),
            last_health: Mutex::new(None),
            blocked_reason: Mutex::new(None),
            health_task: Mutex::new(None),
            stop_health: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn status(&self) -> ServiceStatus {
        *self.status.lock().unwrap()
    }

    fn set_status(&self, s: ServiceStatus) {
        *self.status.lock().unwrap() = s;
    }
}

/// A read-only view of a service's current state, cheap to clone for the
/// CLI/TUI to render.
#[derive(Debug, Clone)]
pub struct ServiceView {
    pub name: String,
    pub status: ServiceStatus,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub pid: Option<i32>,
    pub uptime: Option<Duration>,
    pub health_detail: Option<String>,
    pub blocked_reason: Option<String>,
}

pub struct Manager {
    pub config: Config,
    pub root_dir: PathBuf,
    services: HashMap<String, Arc<ServiceRuntime>>,
}

impl Manager {
    pub fn new(loaded: Loaded) -> Self {
        let services = loaded
            .config
            .services
            .keys()
            .map(|name| (name.clone(), Arc::new(ServiceRuntime::new())))
            .collect();
        Manager {
            config: loaded.config,
            root_dir: loaded.root_dir,
            services,
        }
    }

    pub fn service_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.services.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn logs(&self, name: &str) -> Option<Arc<LogBuffer>> {
        self.services.get(name).map(|s| s.logs.clone())
    }

    pub fn view(&self, name: &str) -> Option<ServiceView> {
        let rt = self.services.get(name)?;
        let svc_cfg = self.config.services.get(name)?;
        let pid = rt.process.lock().unwrap().as_ref().map(|p| p.pid);
        let uptime = rt.started_at.lock().unwrap().map(|t| t.elapsed());
        let health_detail = rt
            .last_health
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| h.detail.clone());
        Some(ServiceView {
            name: name.to_string(),
            status: rt.status(),
            port: svc_cfg.port,
            url: svc_cfg.url.clone(),
            pid,
            uptime,
            health_detail,
            blocked_reason: rt.blocked_reason.lock().unwrap().clone(),
        })
    }

    pub fn all_views(&self) -> Vec<ServiceView> {
        self.service_names()
            .iter()
            .filter_map(|n| self.view(n))
            .collect()
    }

    fn repo_root(&self, repo: &str) -> Option<PathBuf> {
        self.config
            .repositories
            .get(repo)
            .map(|r| crate::config::loader::expand_home(&r.path))
    }

    fn service_cwd(&self, name: &str) -> Result<PathBuf> {
        let svc = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        let base = match &svc.repository {
            Some(repo) => self.repo_root(repo).ok_or_else(|| {
                anyhow!("service '{name}' references unknown repository '{repo}'")
            })?,
            None => self.root_dir.clone(),
        };
        Ok(match &svc.cwd {
            Some(cwd) => base.join(cwd),
            None => base,
        })
    }

    /// Start a template: resolve dependencies and start everything in order,
    /// everything in order, waiting for each service's readiness (healthy
    /// when it has a health check, otherwise just started) before starting
    /// dependents.
    pub async fn start_template(&self, template: &str) -> Result<Vec<String>> {
        let order = graph::services_for_template(&self.config, template)?;
        self.start_ordered(&order).await
    }

    pub async fn start_services(&self, names: &[String]) -> Result<Vec<String>> {
        let order = graph::resolve_start_order(&self.config, names)?;
        self.start_ordered(&order).await
    }

    async fn start_ordered(&self, order: &[String]) -> Result<Vec<String>> {
        let mut started = Vec::new();
        let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

        for name in order {
            let svc = self
                .config
                .services
                .get(name)
                .ok_or_else(|| anyhow!("unknown service '{name}'"))?;

            let blocked_dep = svc.depends_on.iter().find(|d| failed.contains(*d));
            if let Some(dep) = blocked_dep {
                self.mark_blocked(name, &format!("dependency '{dep}' is not available"));
                failed.insert(name.clone());
                continue;
            }

            match self.start_service(name).await {
                Ok(()) => started.push(name.clone()),
                Err(e) => {
                    self.mark_blocked(name, &e.to_string());
                    failed.insert(name.clone());
                }
            }
        }

        if failed.is_empty() {
            Ok(started)
        } else {
            Err(anyhow!(
                "{} service(s) failed or were blocked: {}",
                failed.len(),
                failed.into_iter().collect::<Vec<_>>().join(", ")
            ))
        }
    }

    fn mark_blocked(&self, name: &str, reason: &str) {
        if let Some(rt) = self.services.get(name) {
            rt.set_status(ServiceStatus::Blocked);
            *rt.blocked_reason.lock().unwrap() = Some(reason.to_string());
        }
    }

    /// RF-03: start a single service (assumes its dependencies are already
    /// satisfied — callers going through `start_profile`/`start_services`
    /// handle ordering).
    #[async_recursion::async_recursion]
    pub async fn start_service(&self, name: &str) -> Result<()> {
        let rt = self
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?
            .clone();

        if rt.status().is_started() {
            return Ok(()); // already running
        }

        let svc = self
            .config
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?
            .clone();

        // wait for dependencies to reach their required readiness
        for dep in &svc.depends_on {
            self.wait_for_dependency_ready(dep).await?;
        }

        if let Some(port) = svc.port {
            if let Some(occupant) = portcheck::check_port(port) {
                let who = occupant
                    .pid
                    .map(|pid| {
                        format!(
                            "PID {pid}{}",
                            occupant
                                .process_name
                                .map(|n| format!(" ({n})"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "another process".to_string());
                return Err(anyhow!("port {port} is already in use by {who}"));
            }
        }

        let command = svc
            .command
            .clone()
            .ok_or_else(|| anyhow!("service '{name}' has no command configured"))?;
        let cwd = self.service_cwd(name)?;

        rt.set_status(ServiceStatus::Starting);
        *rt.blocked_reason.lock().unwrap() = None;
        rt.logs.push(LogLine {
            timestamp: chrono::Local::now(),
            service: name.to_string(),
            stream: LogStream::System,
            content: format!("starting: {command} (cwd: {})", cwd.display()),
        });

        let process = RunningProcess::spawn(name, &command, &cwd, &svc.env, rt.logs.clone())?;
        *rt.process.lock().unwrap() = Some(process.clone());
        *rt.started_at.lock().unwrap() = Some(Instant::now());
        rt.set_status(ServiceStatus::Running);

        if let Some(hc) = svc.healthcheck.clone() {
            self.spawn_health_loop(name.to_string(), rt.clone(), hc);
        }

        Ok(())
    }

    #[async_recursion::async_recursion]
    async fn wait_for_dependency_ready(&self, dep: &str) -> Result<()> {
        let dep_svc = self
            .config
            .services
            .get(dep)
            .ok_or_else(|| anyhow!("unknown dependency '{dep}'"))?;
        let mode = dep_svc
            .depends_on_ready
            .unwrap_or(if dep_svc.healthcheck.is_some() {
                ReadyMode::Healthy
            } else {
                ReadyMode::Started
            });

        let rt = self
            .services
            .get(dep)
            .ok_or_else(|| anyhow!("unknown dependency '{dep}'"))?;

        if !rt.status().is_started() {
            self.start_service(dep).await?;
        }

        if mode == ReadyMode::Started {
            return Ok(());
        }

        // ReadyMode::Healthy: poll using the dependency's own healthcheck
        // config, bounded by its configured retries.
        if let Some(hc) = dep_svc.healthcheck.clone() {
            let attempts = health::retries(&hc).max(1);
            let wait = health::interval(&hc);
            for _ in 0..attempts {
                if rt.status() == ServiceStatus::Healthy {
                    return Ok(());
                }
                tokio::time::sleep(wait).await;
            }
            if rt.status() != ServiceStatus::Healthy {
                return Err(anyhow!("dependency '{dep}' did not become healthy"));
            }
        }
        Ok(())
    }

    fn spawn_health_loop(&self, _name: String, rt: Arc<ServiceRuntime>, hc: HealthcheckConfig) {
        rt.stop_health.store(false, Ordering::SeqCst);
        let stop_flag = rt.stop_health.clone();
        let task_rt = rt.clone();
        let handle = tokio::spawn(async move {
            let rt = task_rt;
            // initial readiness probe
            let initial = health::wait_until_healthy(&hc).await;
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            if initial {
                rt.set_status(ServiceStatus::Healthy);
            } else if rt.status() != ServiceStatus::Failed {
                rt.set_status(ServiceStatus::Unhealthy);
            }

            let interval = health::interval(&hc);
            loop {
                tokio::time::sleep(interval).await;
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                if !rt.status().is_started() {
                    return; // process stopped/crashed, health loop no longer relevant
                }
                let result = health::check_once(&hc).await;
                let ok = result.is_ok();
                let detail = match result {
                    Ok(()) => "ok".to_string(),
                    Err(e) => e,
                };
                *rt.last_health.lock().unwrap() = Some(HealthResult {
                    ok,
                    detail,
                    checked_at: Instant::now(),
                });
                if rt.status().is_started() {
                    rt.set_status(if ok {
                        ServiceStatus::Healthy
                    } else {
                        ServiceStatus::Unhealthy
                    });
                }
            }
        });
        *rt.health_task.lock().unwrap() = Some(handle);
    }

    /// RF-07/RF-09: stop a single service, terminating its whole process
    /// group (graceful, then forced after a timeout).
    pub async fn stop_service(&self, name: &str) -> Result<()> {
        let rt = self
            .services
            .get(name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?
            .clone();

        rt.stop_health.store(true, Ordering::SeqCst);
        if let Some(handle) = rt.health_task.lock().unwrap().take() {
            handle.abort();
        }

        let process = rt.process.lock().unwrap().take();
        if let Some(process) = process {
            rt.set_status(ServiceStatus::Stopping);
            process.stop(GRACEFUL_STOP_TIMEOUT).await?;
        }
        rt.set_status(ServiceStatus::Stopped);
        *rt.started_at.lock().unwrap() = None;
        Ok(())
    }

    pub async fn restart_service(&self, name: &str) -> Result<()> {
        self.stop_service(name).await?;
        self.start_service(name).await
    }

    /// RF-08: stop everything, dependents-first.
    pub async fn stop_all(&self) -> Result<()> {
        let running: Vec<String> = self
            .service_names()
            .into_iter()
            .filter(|n| {
                self.services
                    .get(n)
                    .map(|rt| rt.status() != ServiceStatus::Stopped)
                    .unwrap_or(false)
            })
            .collect();
        let order = graph::resolve_start_order(&self.config, &running).unwrap_or(running);
        for name in graph::stop_order(&order) {
            let _ = self.stop_service(&name).await;
        }
        Ok(())
    }

    /// Any service currently running or starting (used to decide whether to
    /// prompt on quit — RF-08).
    pub fn any_running(&self) -> bool {
        self.services
            .values()
            .any(|rt| rt.status() != ServiceStatus::Stopped)
    }

    #[allow(dead_code)]
    pub fn crashed_services(&self) -> Vec<String> {
        self.services
            .iter()
            .filter(|(_, rt)| rt.status() == ServiceStatus::Failed)
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Poll running processes for unexpected exits and flip their status to
    /// `Failed` (RNF-04: a crash must not bring down `hum` itself).
    pub fn reap_exited(&self) {
        for (name, rt) in &self.services {
            let mut proc_guard = rt.process.lock().unwrap();
            if let Some(process) = proc_guard.as_ref() {
                if !process.is_alive() && rt.status().is_started() {
                    let code = process.exit_code().unwrap_or(-1);
                    rt.set_status(ServiceStatus::Failed);
                    *rt.blocked_reason.lock().unwrap() =
                        Some(format!("process exited with code {code}"));
                    rt.logs.push(LogLine {
                        timestamp: chrono::Local::now(),
                        service: name.clone(),
                        stream: LogStream::System,
                        content: format!("crashed with exit code {code}"),
                    });
                    *proc_guard = None;
                }
            }
        }
    }
}
