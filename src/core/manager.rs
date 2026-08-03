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
use super::state::{HealthState, PortState, PresentationState, ProcessState, ServiceState};

const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const PID_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PORT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Everything `hum` tracks about one service while it's part of the running
/// session.
pub struct ServiceRuntime {
    pub state: Mutex<ServiceState>,
    pub process: Mutex<Option<Arc<RunningProcess>>>,
    pub logs: Arc<LogBuffer>,
    pub started_at: Mutex<Option<Instant>>,
    health_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    operation: tokio::sync::Mutex<()>,
    pid_checked_at: Mutex<Option<Instant>>,
    port_checked_at: Mutex<Option<Instant>>,
}

impl ServiceRuntime {
    fn new() -> Self {
        ServiceRuntime {
            state: Mutex::new(ServiceState::default()),
            process: Mutex::new(None),
            logs: LogBuffer::new(crate::runtime::logs::DEFAULT_CAPACITY),
            started_at: Mutex::new(None),
            health_task: Mutex::new(None),
            operation: tokio::sync::Mutex::new(()),
            pid_checked_at: Mutex::new(None),
            port_checked_at: Mutex::new(None),
        }
    }

    pub fn state(&self) -> ServiceState {
        self.state.lock().unwrap().clone()
    }
}

/// A read-only view of a service's current state, cheap to clone for the
/// CLI/TUI to render.
#[derive(Debug, Clone)]
pub struct ServiceView {
    pub process: ProcessState,
    pub port_state: PortState,
    pub health: HealthState,
    pub presentation: PresentationState,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub pid: Option<i32>,
    pub uptime: Option<Duration>,
    pub exit_code: Option<i32>,
    pub changed_at: chrono::DateTime<chrono::Utc>,
    pub health_detail: Option<String>,
    pub health_duration_ms: Option<u64>,
    pub last_error: Option<String>,
}

pub struct Manager {
    pub config: Config,
    pub root_dir: PathBuf,
    env_overrides: HashMap<String, String>,
    services: HashMap<String, Arc<ServiceRuntime>>,
    poll_in_flight: AtomicBool,
}

impl Manager {
    pub fn with_env(loaded: Loaded, env_overrides: HashMap<String, String>) -> Self {
        let services = loaded
            .config
            .services
            .keys()
            .map(|name| (name.clone(), Arc::new(ServiceRuntime::new())))
            .collect();
        Manager {
            config: loaded.config,
            root_dir: loaded.root_dir,
            env_overrides,
            services,
            poll_in_flight: AtomicBool::new(false),
        }
    }

    pub fn service_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.services.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn env_overrides(&self) -> &HashMap<String, String> {
        &self.env_overrides
    }

    pub fn logs(&self, name: &str) -> Option<Arc<LogBuffer>> {
        self.services.get(name).map(|s| s.logs.clone())
    }

    pub fn view(&self, name: &str) -> Option<ServiceView> {
        let rt = self.services.get(name)?;
        let svc_cfg = self.config.services.get(name)?;
        let pid = rt.process.lock().unwrap().as_ref().map(|p| p.pid);
        let uptime = rt.started_at.lock().unwrap().map(|t| t.elapsed());
        let state = rt.state();
        let presentation = state.presentation();
        Some(ServiceView {
            process: state.process,
            port_state: state.port,
            health: state.health,
            presentation,
            port: svc_cfg.port,
            url: svc_cfg.url.clone(),
            pid,
            uptime,
            exit_code: state.exit_code,
            changed_at: state.changed_at,
            health_detail: state.health_detail,
            health_duration_ms: state.last_health_duration_ms,
            last_error: state.last_error,
        })
    }

    fn repo_root(&self, repo: &str) -> Option<PathBuf> {
        self.config.repositories.get(repo).map(|r| {
            let path = crate::config::loader::expand_home(&r.path);
            if path.is_absolute() {
                path
            } else {
                self.root_dir.join(path)
            }
        })
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
    /// everything in order, waiting for each service's explicit readiness:
    /// health when configured, otherwise its port when configured, otherwise
    /// the running process.
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
            rt.state
                .lock()
                .unwrap()
                .mark_start_failed(reason.to_string());
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
        let _operation = rt.operation.lock().await;

        self.reconcile_process(name);
        if let Some(process) = rt.process.lock().unwrap().as_ref() {
            if process.is_alive() && rt.state().process.is_running() {
                return Ok(()); // already running
            }
            if process.is_alive() {
                return Err(anyhow!(
                    "service '{name}' still has an active process while {}",
                    rt.state().process.label()
                ));
            }
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

        let generation = rt.state.lock().unwrap().begin_start();

        if let Some(port) = svc.port {
            if let Some(occupant) = portcheck::check_port(port) {
                rt.state.lock().unwrap().port = PortState::OccupiedByOther {
                    pid: occupant.pid,
                    process_name: occupant.process_name.clone(),
                };
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
            rt.state.lock().unwrap().port = PortState::Closed;
            *rt.port_checked_at.lock().unwrap() = None;
        } else {
            rt.state.lock().unwrap().port = PortState::Unknown;
        }

        let command = svc
            .command
            .clone()
            .ok_or_else(|| anyhow!("service '{name}' has no command configured"))?;
        let cwd = self.service_cwd(name)?;
        let env = crate::config::environment::resolve_service_env(&svc, &cwd, &self.env_overrides)?;

        rt.logs.push(LogLine {
            timestamp: chrono::Local::now(),
            service: name.to_string(),
            stream: LogStream::System,
            content: format!("starting: {command} (cwd: {})", cwd.display()),
        });

        let process = RunningProcess::spawn(name, &command, &cwd, &env, rt.logs.clone())?;
        *rt.process.lock().unwrap() = Some(process.clone());
        *rt.started_at.lock().unwrap() = Some(Instant::now());
        rt.state
            .lock()
            .unwrap()
            .mark_running(generation, svc.healthcheck.is_some());

        if let Some(hc) = svc.healthcheck.clone() {
            self.spawn_health_loop(rt.clone(), process, hc, generation);
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
            } else if dep_svc.port.is_some() {
                ReadyMode::Listening
            } else {
                ReadyMode::Started
            });

        let rt = self
            .services
            .get(dep)
            .ok_or_else(|| anyhow!("unknown dependency '{dep}'"))?;

        if !rt.state().process.is_running() {
            self.start_service(dep).await?;
        }

        if mode == ReadyMode::Started {
            return Ok(());
        }

        let (attempts, wait) = match (&mode, &dep_svc.healthcheck) {
            (ReadyMode::Healthy, Some(hc)) => (health::retries(hc).max(1), health::interval(hc)),
            (ReadyMode::Healthy, None) => {
                return Err(anyhow!(
                    "dependency '{dep}' requires healthy readiness but has no healthcheck"
                ));
            }
            (ReadyMode::Listening, _) => (50, Duration::from_millis(200)),
            (ReadyMode::Started, _) => unreachable!(),
        };
        for _ in 0..attempts {
            if !self.reconcile_process(dep) {
                return Err(anyhow!(
                    "dependency '{dep}' process exited before readiness"
                ));
            }
            self.update_port_state(dep, true);
            let state = rt.state();
            if !state.process.is_running() {
                return Err(anyhow!("dependency '{dep}' process is not running"));
            }
            let ready = match mode {
                ReadyMode::Started => true,
                ReadyMode::Listening => state.port == PortState::Listening,
                ReadyMode::Healthy => state.health == HealthState::Healthy,
            };
            if ready {
                return Ok(());
            }
            tokio::time::sleep(wait).await;
        }
        Err(anyhow!(
            "dependency '{dep}' did not become {}",
            match mode {
                ReadyMode::Started => "started",
                ReadyMode::Listening => "listening",
                ReadyMode::Healthy => "healthy",
            }
        ))
    }

    fn spawn_health_loop(
        &self,
        rt: Arc<ServiceRuntime>,
        process: Arc<RunningProcess>,
        hc: HealthcheckConfig,
        generation: u64,
    ) {
        let task_runtime = rt.clone();
        let handle = tokio::spawn(async move {
            let interval = health::interval(&hc);
            loop {
                let started = Instant::now();
                let result = health::check_once(&hc).await;
                let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                if !process.is_alive() {
                    let code = process.exit_code();
                    task_runtime
                        .state
                        .lock()
                        .unwrap()
                        .mark_exited_for_generation(
                            generation,
                            code,
                            code.map(|code| format!("process exited with code {code}"))
                                .unwrap_or_else(|| "process exited".to_string()),
                        );
                    return;
                }
                let (health, detail) = match result {
                    Ok(()) => (HealthState::Healthy, "ok".to_string()),
                    Err(error) => (HealthState::Unhealthy, error),
                };
                if !task_runtime.state.lock().unwrap().apply_health(
                    generation,
                    health,
                    detail,
                    duration_ms,
                ) {
                    return;
                }
                tokio::time::sleep(interval).await;
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
        let _operation = rt.operation.lock().await;

        rt.state.lock().unwrap().begin_stop();
        if let Some(handle) = rt.health_task.lock().unwrap().take() {
            handle.abort();
        }

        let process = rt.process.lock().unwrap().clone();
        if let Some(process) = process {
            process.stop(GRACEFUL_STOP_TIMEOUT).await?;
            rt.state.lock().unwrap().exit_code = process.exit_code();
            *rt.process.lock().unwrap() = None;
        }
        rt.state.lock().unwrap().mark_missing();
        *rt.started_at.lock().unwrap() = None;
        self.update_port_state(name, false);
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
                    .map(|rt| rt.state().process.is_active())
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
            .any(|rt| rt.state().process.is_active())
    }

    #[allow(dead_code)]
    pub fn crashed_services(&self) -> Vec<String> {
        self.services
            .iter()
            .filter(|(_, rt)| rt.state().process == ProcessState::Exited)
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Poll running processes for unexpected exits and flip their status to
    /// `Failed` (RNF-04: a crash must not bring down `hum` itself).
    pub fn reap_exited(&self) {
        for name in self.services.keys() {
            let Some(runtime) = self.services.get(name) else {
                continue;
            };
            if poll_is_due(&runtime.pid_checked_at, PID_POLL_INTERVAL) {
                self.reconcile_process(name);
            }
            if poll_is_due(&runtime.port_checked_at, PORT_POLL_INTERVAL) {
                self.update_port_state(name, false);
            }
        }
    }

    pub fn schedule_poll(self: &Arc<Self>) {
        if self
            .poll_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            manager.reap_exited();
            manager.poll_in_flight.store(false, Ordering::Release);
        });
    }

    fn reconcile_process(&self, name: &str) -> bool {
        let Some(rt) = self.services.get(name) else {
            return false;
        };
        let mut process_guard = rt.process.lock().unwrap();
        let Some(process) = process_guard.as_ref() else {
            return false;
        };
        if process.is_alive() {
            return true;
        }

        let code = process.exit_code();
        let detail = code
            .map(|code| format!("process exited with code {code}"))
            .unwrap_or_else(|| "process exited".to_string());
        rt.state.lock().unwrap().mark_exited(code, detail.clone());
        if let Some(handle) = rt.health_task.lock().unwrap().take() {
            handle.abort();
        }
        rt.logs.push(LogLine {
            timestamp: chrono::Local::now(),
            service: name.to_string(),
            stream: LogStream::System,
            content: detail,
        });
        *process_guard = None;
        false
    }

    fn update_port_state(&self, name: &str, _force_owner_check: bool) {
        let Some(rt) = self.services.get(name) else {
            return;
        };
        let Some(port) = self
            .config
            .services
            .get(name)
            .and_then(|service| service.port)
        else {
            rt.state.lock().unwrap().port = PortState::Unknown;
            return;
        };
        let snapshot = rt.state();
        let host = self
            .config
            .services
            .get(name)
            .and_then(|service| service.url.as_deref())
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "localhost".to_string());
        let state = match portcheck::probe_host_port(&host, port, Duration::from_millis(50)) {
            portcheck::PortProbe::Listening if snapshot.process.is_running() => {
                // Start already diagnosed the port as free. Ordinary polling
                // never shells out; explicit status/doctor handles ownership
                // diagnostics when the state is unknown or contradictory.
                PortState::Listening
            }
            portcheck::PortProbe::Listening => PortState::OccupiedByOther {
                pid: None,
                process_name: None,
            },
            portcheck::PortProbe::Closed => PortState::Closed,
            portcheck::PortProbe::Unknown => PortState::Unknown,
        };
        rt.state.lock().unwrap().port = state;
    }
}

fn poll_is_due(last: &Mutex<Option<Instant>>, interval: Duration) -> bool {
    let mut last = last.lock().unwrap();
    if last.is_some_and(|instant| instant.elapsed() < interval) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::config::{RepositoryConfig, ServiceConfig};

    #[test]
    fn resolves_relative_repository_from_config_directory() {
        let root = PathBuf::from("/tmp/hum-project");
        let loaded = Loaded {
            config: Config {
                repositories: HashMap::from([(
                    "api".to_string(),
                    RepositoryConfig {
                        path: "../api".into(),
                    },
                )]),
                services: HashMap::from([(
                    "server".to_string(),
                    ServiceConfig {
                        repository: Some("api".to_string()),
                        cwd: Some("src".into()),
                        command: Some("true".to_string()),
                        ..ServiceConfig::default()
                    },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root.clone(),
        };
        let manager = Manager::with_env(loaded, HashMap::new());

        assert_eq!(
            manager.service_cwd("server").unwrap(),
            root.join("../api/src")
        );
    }

    #[tokio::test]
    async fn stop_cancels_in_flight_health_task_and_invalidates_generation() {
        let root = PathBuf::from("/tmp/hum-project");
        let loaded = Loaded {
            config: Config {
                services: HashMap::from([(
                    "server".to_string(),
                    ServiceConfig {
                        command: Some("true".to_string()),
                        ..ServiceConfig::default()
                    },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root,
        };
        let manager = Manager::with_env(loaded, HashMap::new());
        let runtime = manager.services.get("server").unwrap();
        let generation = runtime.state.lock().unwrap().begin_start();
        runtime.state.lock().unwrap().mark_running(generation, true);

        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = completed.clone();
        *runtime.health_task.lock().unwrap() = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            task_completed.store(true, Ordering::SeqCst);
        }));

        manager.stop_service("server").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let state = runtime.state();
        assert!(!completed.load(Ordering::SeqCst));
        assert!(state.generation > generation);
        assert_eq!(state.process, ProcessState::Missing);
        assert_eq!(state.health, HealthState::Unchecked);
    }

    #[tokio::test]
    async fn concurrent_starts_create_only_one_generation() {
        let root = std::env::temp_dir();
        let loaded = Loaded {
            config: Config {
                services: HashMap::from([(
                    "server".to_string(),
                    ServiceConfig {
                        command: Some("sleep 5".to_string()),
                        ..ServiceConfig::default()
                    },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root,
        };
        let manager = Arc::new(Manager::with_env(loaded, HashMap::new()));
        let first = manager.clone();
        let second = manager.clone();
        let (first_result, second_result) = tokio::join!(
            first.start_service("server"),
            second.start_service("server")
        );

        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert_eq!(manager.services["server"].state().generation, 1);
        assert!(manager.view("server").unwrap().pid.is_some());
        manager.stop_service("server").await.unwrap();
    }

    #[test]
    fn ordinary_port_poll_does_not_run_occupant_diagnostics() {
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let root = std::env::temp_dir();
        let loaded = Loaded {
            config: Config {
                services: HashMap::from([(
                    "server".to_string(),
                    ServiceConfig {
                        command: Some("true".to_string()),
                        port: Some(port),
                        url: Some(format!("http://127.0.0.1:{port}")),
                        ..ServiceConfig::default()
                    },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root,
        };
        let manager = Manager::with_env(loaded, HashMap::new());
        let runtime = &manager.services["server"];
        let generation = runtime.state.lock().unwrap().begin_start();
        runtime
            .state
            .lock()
            .unwrap()
            .mark_running(generation, false);
        portcheck::reset_diagnostic_call_count();

        manager.update_port_state("server", false);

        assert_eq!(runtime.state().port, PortState::Listening);
        assert_eq!(portcheck::diagnostic_call_count(), 0);
    }
}
