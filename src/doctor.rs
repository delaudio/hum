use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::loader::expand_home;
use crate::config::{Config, EnvironmentProviderConfig, RuntimeConfig};
use crate::runtime::detached::DetachedRuntime;
use crate::runtime::portcheck;
use crate::runtime::project::ProjectRuntime;
use crate::runtime::registry::{inspect_identity, IdentityStatus, RuntimeEntry};

/// One diagnostic result (section 8.5 / RF-16). `scope` is `None` for
/// repository/global checks, `Some(service)` for per-service checks.
#[derive(Debug, Clone)]
pub struct DoctorCheck {
    pub scope: Option<String>,
    pub label: String,
    pub ok: bool,
    pub detail: Option<String>,
}

impl DoctorCheck {
    fn ok(scope: Option<&str>, label: impl Into<String>) -> Self {
        DoctorCheck {
            scope: scope.map(str::to_string),
            label: label.into(),
            ok: true,
            detail: None,
        }
    }
    fn fail(scope: Option<&str>, label: impl Into<String>, detail: impl Into<String>) -> Self {
        DoctorCheck {
            scope: scope.map(str::to_string),
            label: label.into(),
            ok: false,
            detail: Some(detail.into()),
        }
    }
}

pub fn run_with_project(runtime: &ProjectRuntime) -> Vec<DoctorCheck> {
    let selection = runtime
        .config()
        .services
        .keys()
        .chain(runtime.config().tasks.keys())
        .cloned()
        .collect::<Vec<_>>();
    run_with_project_selection(runtime, &selection)
}

pub fn run_with_project_selection(
    runtime: &ProjectRuntime,
    selection: &[String],
) -> Vec<DoctorCheck> {
    let selected = selection.iter().cloned().collect::<HashSet<_>>();
    let process = runtime.process_runtime();
    let mut results = run_checks(
        process.config(),
        process.root_dir(),
        process.env_overrides(),
        process,
        &selected,
    );
    let config = runtime.config();
    let selected_runtimes = config
        .services
        .iter()
        .filter(|(name, _)| selected.contains(*name))
        .filter_map(|(_, service)| service.runtime.as_ref())
        .collect::<HashSet<_>>();
    if selected_runtimes.iter().any(|name| {
        matches!(
            config.runtimes.get(*name),
            Some(RuntimeConfig::Compose { .. })
        )
    }) {
        results.push(command_check(
            "Docker daemon available",
            "docker",
            &["info"],
        ));
        results.push(command_check(
            "Docker Compose v2 available",
            "docker",
            &["compose", "version"],
        ));
    }
    for (name, adapter) in &config.runtimes {
        if !selected_runtimes.contains(name) {
            continue;
        }
        if let RuntimeConfig::Compose {
            files,
            generated_files,
            env_file,
            ..
        } = adapter
        {
            for file in files {
                let path = absolute_from(process.root_dir(), file);
                results.push(file_check(
                    Some(name),
                    "Compose file",
                    &path,
                    path.is_file(),
                ));
            }
            for file in generated_files {
                let path = absolute_from(process.root_dir(), file);
                results.push(DoctorCheck::ok(
                    Some(name),
                    if path.is_file() {
                        format!("Generated Compose file found ({})", path.display())
                    } else {
                        format!(
                            "Generated Compose file not present yet ({})",
                            path.display()
                        )
                    },
                ));
            }
            if let Some(file) = env_file {
                let path = absolute_from(process.root_dir(), file);
                results.push(file_check(
                    Some(name),
                    "Compose env file",
                    &path,
                    path.is_file(),
                ));
            }
        }
    }
    let selected_providers = selection
        .iter()
        .flat_map(|name| {
            config
                .services
                .get(name)
                .map(|service| service.env_from.as_slice())
                .or_else(|| config.tasks.get(name).map(|task| task.env_from.as_slice()))
                .unwrap_or_default()
        })
        .map(|source| source.provider.as_str())
        .collect::<HashSet<_>>();
    for (name, provider) in &config.environment_providers {
        if !selected_providers.contains(name.as_str()) {
            continue;
        }
        match provider {
            EnvironmentProviderConfig::OnePassword { .. } => {
                if which::which("op").is_ok() {
                    results.push(DoctorCheck::ok(
                        Some(name),
                        "1Password CLI available (items not read)",
                    ));
                } else {
                    results.push(DoctorCheck::fail(
                        Some(name),
                        "1Password CLI available",
                        "`op` not found on PATH; doctor did not attempt to read any item",
                    ));
                }
            }
        }
    }
    let mut tasks = config.tasks.iter().collect::<Vec<_>>();
    tasks.sort_by_key(|(name, _)| *name);
    for (name, task) in tasks {
        if !selected.contains(name) {
            continue;
        }
        let cwd = task
            .cwd
            .as_ref()
            .map(|path| absolute_from(process.root_dir(), path))
            .unwrap_or_else(|| process.root_dir().to_path_buf());
        results.push(file_check(
            Some(name),
            "Task working directory",
            &cwd,
            cwd.is_dir(),
        ));
        results.push(task_command_check(
            name,
            "Task command",
            &task.command[0],
            &cwd,
        ));
        if let Some(check) = &task.check {
            results.push(task_command_check(
                name,
                "Task idempotency check",
                &check[0],
                &cwd,
            ));
        }
        if let Some(doctor) = &task.doctor {
            // `selected` is the resolved dependency graph, so out-of-scope
            // task hooks never run. Provider values are intentionally skipped:
            // diagnostics receive only reviewed, literal task environment.
            results.push(task_doctor_check(name, doctor, &cwd, &task.env));
        }
    }
    results
}

fn task_doctor_check(
    scope: &str,
    argv: &[String],
    cwd: &std::path::Path,
    environment: &HashMap<String, String>,
) -> DoctorCheck {
    let Some((program, arguments)) = argv.split_first() else {
        return DoctorCheck::fail(Some(scope), "Task doctor command", "argv is empty");
    };
    match Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .envs(environment)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            DoctorCheck::ok(Some(scope), "Task doctor command passed")
        }
        Ok(status) => DoctorCheck::fail(
            Some(scope),
            "Task doctor command",
            format!("command exited with status {status}"),
        ),
        Err(error) => DoctorCheck::fail(
            Some(scope),
            "Task doctor command",
            format!("command could not be started: {error}"),
        ),
    }
}

fn task_command_check(
    scope: &str,
    label: &str,
    program: &str,
    cwd: &std::path::Path,
) -> DoctorCheck {
    let path = std::path::Path::new(program);
    let resolved = if path.components().count() > 1 {
        let candidate = absolute_from(cwd, path);
        candidate.is_file().then_some(candidate)
    } else {
        which::which(program).ok()
    };
    match resolved {
        Some(path) => DoctorCheck::ok(Some(scope), format!("{label} found ({})", path.display())),
        None => DoctorCheck::fail(Some(scope), label, format!("command not found: {program}")),
    }
}

fn run_checks(
    config: &Config,
    root_dir: &std::path::Path,
    cli_overrides: &HashMap<String, String>,
    runtime: &DetachedRuntime,
    selected: &HashSet<String>,
) -> Vec<DoctorCheck> {
    let mut results = Vec::new();
    let running_compose_targets = inspect_running_compose_targets(config, selected);

    // Repositories
    let selected_repositories = config
        .services
        .iter()
        .filter(|(name, _)| selected.contains(*name))
        .filter_map(|(_, service)| service.repository.as_deref())
        .collect::<HashSet<_>>();
    let mut repositories = config
        .repositories
        .iter()
        .filter(|(name, _)| selected_repositories.contains(name.as_str()))
        .collect::<Vec<_>>();
    repositories.sort_by_key(|(name, _)| *name);
    for (name, repo) in repositories {
        let path = expand_home(&repo.path);
        let path = if path.is_absolute() {
            path
        } else {
            root_dir.join(path)
        };
        if path.is_dir() {
            results.push(DoctorCheck::ok(
                None,
                format!("Repository '{name}' found ({})", path.display()),
            ));
        } else {
            results.push(DoctorCheck::fail(
                None,
                format!("Repository '{name}'"),
                format!("directory not found: {}", path.display()),
            ));
        }
    }

    // Services
    let mut services = config.services.iter().collect::<Vec<_>>();
    services.sort_by_key(|(name, _)| *name);
    for (name, svc) in services {
        if !selected.contains(name) {
            continue;
        }
        let process_owned = runtime.owns_service(name);
        let (runtime_entry, registry_read_failed) = if process_owned {
            match runtime.registry().load(name) {
                Ok(entry) => (entry, false),
                Err(error) => {
                    results.push(DoctorCheck::fail(
                        Some(name),
                        "Runtime registry",
                        error.to_string(),
                    ));
                    (None, true)
                }
            }
        } else {
            results.push(DoctorCheck::ok(
                Some(name),
                format!(
                    "External runtime: {}",
                    svc.runtime.as_deref().unwrap_or("unknown")
                ),
            ));
            (None, true)
        };
        let runtime_identity = runtime_entry
            .as_ref()
            .map(|entry| (entry, inspect_identity(entry)));
        if !registry_read_failed {
            match &runtime_identity {
                None => results.push(DoctorCheck::ok(
                    Some(name),
                    "Runtime registry: no managed process",
                )),
                Some((entry, IdentityStatus::Matching)) => results.push(DoctorCheck::ok(
                    Some(name),
                    format!(
                        "Runtime registry: managed by hum (PID {}, PGID {})",
                        entry.pid, entry.pgid
                    ),
                )),
                Some((_, IdentityStatus::Missing)) => results.push(DoctorCheck::fail(
                    Some(name),
                    "Runtime registry stale",
                    "registered process is no longer present",
                )),
                Some((_, IdentityStatus::Mismatch(reason))) => results.push(DoctorCheck::fail(
                    Some(name),
                    "Runtime registry identity mismatch",
                    reason.clone(),
                )),
            }
        }
        let base = svc
            .repository
            .as_ref()
            .and_then(|r| config.repositories.get(r))
            .map(|r| {
                let path = expand_home(&r.path);
                if path.is_absolute() {
                    path
                } else {
                    root_dir.join(path)
                }
            })
            .unwrap_or_else(|| root_dir.to_path_buf());
        let cwd = match &svc.cwd {
            Some(c) => base.join(c),
            None => base.clone(),
        };

        if cwd.is_dir() {
            results.push(DoctorCheck::ok(
                Some(name),
                format!("Working directory found ({})", cwd.display()),
            ));
        } else {
            results.push(DoctorCheck::fail(
                Some(name),
                "Working directory",
                format!("not found: {}", cwd.display()),
            ));
        }

        let resolved_env =
            match crate::config::environment::resolve_service_env(svc, &cwd, cli_overrides) {
                Ok(env) => {
                    if let Some(env_file) = &svc.env_file {
                        let path = if env_file.is_absolute() {
                            env_file.clone()
                        } else {
                            cwd.join(env_file)
                        };
                        results.push(DoctorCheck::ok(
                            Some(name),
                            format!("Env file found ({})", path.display()),
                        ));
                    }
                    Some(env)
                }
                Err(error) => {
                    results.push(DoctorCheck::fail(Some(name), "Env file", error.to_string()));
                    None
                }
            };

        for command in &svc.requires.commands {
            if which::which(command).is_ok() {
                results.push(DoctorCheck::ok(Some(name), format!("{command} available")));
            } else {
                results.push(DoctorCheck::fail(
                    Some(name),
                    format!("{command} available"),
                    format!("`{command}` not found on PATH"),
                ));
            }
        }

        for file in &svc.requires.files {
            let path: PathBuf = if file.is_absolute() {
                file.clone()
            } else {
                cwd.join(file)
            };
            if path.is_file() {
                results.push(DoctorCheck::ok(
                    Some(name),
                    format!("{} found", file.display()),
                ));
            } else {
                results.push(DoctorCheck::fail(
                    Some(name),
                    format!("{} found", file.display()),
                    format!("missing file: {}", path.display()),
                ));
            }
        }

        for var in &svc.requires.env {
            if cli_overrides.contains_key(var)
                || std::env::var_os(var).is_some()
                || resolved_env
                    .as_ref()
                    .is_some_and(|environment| environment.contains_key(var))
            {
                results.push(DoctorCheck::ok(Some(name), format!("env {var} set")));
            } else {
                results.push(DoctorCheck::fail(
                    Some(name),
                    format!("env {var} set"),
                    format!("missing environment variable: {var}"),
                ));
            }
        }

        if let Some(port) = svc.port {
            if process_owned {
                check_service_port(name, port, runtime_identity.as_ref(), &mut results);
            } else {
                let runtime_running = svc
                    .runtime
                    .as_ref()
                    .and_then(|runtime_name| running_compose_targets.get(runtime_name))
                    .and_then(Option::as_ref)
                    .and_then(|targets| svc.target.as_ref().map(|target| targets.contains(target)));
                check_external_runtime_port(name, port, runtime_running, &mut results);
            }
        }

        // Convention: if this looks like a Node project (package.json
        // present), node_modules should exist too.
        if cwd.join("package.json").is_file() {
            if cwd.join("node_modules").is_dir() {
                results.push(DoctorCheck::ok(Some(name), "node_modules installed"));
            } else {
                results.push(DoctorCheck::fail(
                    Some(name),
                    "node_modules installed",
                    "package.json found but node_modules is missing — run the package manager install",
                ));
            }
        }
    }

    // Config validity + cycles: if we got this far, `config::validate` has
    // already accepted the config. Report it explicitly for visibility.
    results.push(DoctorCheck::ok(None, "Configuration is valid"));
    results.push(DoctorCheck::ok(None, "No circular dependencies"));

    results
}

fn inspect_running_compose_targets(
    config: &Config,
    selected: &HashSet<String>,
) -> HashMap<String, Option<HashSet<String>>> {
    let mut runtime_names = config
        .services
        .iter()
        .filter(|(name, _)| selected.contains(*name))
        .filter_map(|(_, service)| service.runtime.as_ref())
        .filter(|runtime_name| {
            matches!(
                config.runtimes.get(*runtime_name),
                Some(RuntimeConfig::Compose { .. })
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    runtime_names.sort();
    runtime_names.dedup();

    runtime_names
        .into_iter()
        .map(|runtime_name| {
            let RuntimeConfig::Compose { project_name, .. } = &config.runtimes[&runtime_name]
            else {
                unreachable!("runtime names were filtered to Compose runtimes");
            };
            let output = Command::new("docker")
                .arg("ps")
                .arg("--filter")
                .arg(format!("label=com.docker.compose.project={project_name}"))
                .arg("--format")
                .arg("{{.Label \"com.docker.compose.service\"}}")
                .output();
            let targets = output.ok().and_then(|output| {
                output.status.success().then(|| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::trim)
                        .filter(|target| !target.is_empty())
                        .map(str::to_string)
                        .collect::<HashSet<_>>()
                })
            });
            (runtime_name, targets)
        })
        .collect()
}

fn command_check(label: &str, program: &str, arguments: &[&str]) -> DoctorCheck {
    let success = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if success {
        DoctorCheck::ok(None, label)
    } else {
        DoctorCheck::fail(None, label, format!("`{program}` command failed"))
    }
}

fn absolute_from(root: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn file_check(
    scope: Option<&str>,
    label: &str,
    path: &std::path::Path,
    exists: bool,
) -> DoctorCheck {
    if exists {
        DoctorCheck::ok(scope, format!("{label} found ({})", path.display()))
    } else {
        DoctorCheck::fail(scope, label, format!("missing file: {}", path.display()))
    }
}

fn check_external_runtime_port(
    service: &str,
    port: u16,
    runtime_running: Option<bool>,
    results: &mut Vec<DoctorCheck>,
) {
    let probe = portcheck::probe_port(port);
    let occupants = if probe == portcheck::PortProbe::Listening {
        portcheck::identify_occupants(port)
    } else {
        Vec::new()
    };
    results.push(classify_external_runtime_port(
        service,
        port,
        probe,
        runtime_running,
        &occupants,
    ));
}

fn classify_external_runtime_port(
    service: &str,
    port: u16,
    probe: portcheck::PortProbe,
    runtime_running: Option<bool>,
    occupants: &[portcheck::PortOccupant],
) -> DoctorCheck {
    match probe {
        portcheck::PortProbe::Unknown => DoctorCheck::ok(
            Some(service),
            format!("port {port} ownership deferred to runtime"),
        ),
        portcheck::PortProbe::Closed if runtime_running == Some(true) => DoctorCheck::fail(
            Some(service),
            format!("port {port} listener"),
            "Compose service is running but its published port is not listening",
        ),
        portcheck::PortProbe::Closed => DoctorCheck::ok(
            Some(service),
            format!("port {port} available or not currently published"),
        ),
        portcheck::PortProbe::Listening => {
            let host_occupants = occupants
                .iter()
                .filter(|occupant| !is_container_port_forwarder(occupant))
                .collect::<Vec<_>>();
            if !host_occupants.is_empty() {
                return DoctorCheck::fail(
                    Some(service),
                    format!("port {port} shadowed by host process"),
                    format_occupants(&host_occupants),
                );
            }
            if runtime_running == Some(false) {
                return DoctorCheck::fail(
                    Some(service),
                    format!("port {port} occupied while Compose service is stopped"),
                    format_occupants(&occupants.iter().collect::<Vec<_>>()),
                );
            }
            DoctorCheck::ok(
                Some(service),
                format!("port {port} listening for external runtime"),
            )
        }
    }
}

fn is_container_port_forwarder(occupant: &portcheck::PortOccupant) -> bool {
    let names = [
        occupant.process_name.as_deref(),
        occupant
            .command
            .as_deref()
            .and_then(|command| command.split_whitespace().next()),
    ];
    names.into_iter().flatten().any(|name| {
        let name = name.to_ascii_lowercase();
        ["docker", "vpnkit", "rootlesskit", "slirp4netns", "podman"]
            .iter()
            .any(|runtime| name.contains(runtime))
    })
}

fn format_occupants(occupants: &[&portcheck::PortOccupant]) -> String {
    let known = occupants
        .iter()
        .filter_map(|occupant| {
            let pid = occupant.pid?;
            Some(match occupant.process_name.as_deref() {
                Some(name) => format!("PID {pid} ({name})"),
                None => format!("PID {pid}"),
            })
        })
        .collect::<Vec<_>>();
    if known.is_empty() {
        "listener owner could not be identified".to_string()
    } else {
        known.join(", ")
    }
}

pub fn all_passed(results: &[DoctorCheck]) -> bool {
    results.iter().all(|r| r.ok)
}

fn check_service_port(
    service: &str,
    port: u16,
    runtime_identity: Option<&(&RuntimeEntry, IdentityStatus)>,
    results: &mut Vec<DoctorCheck>,
) {
    let listening = match portcheck::probe_port(port) {
        portcheck::PortProbe::Listening => true,
        portcheck::PortProbe::Closed => false,
        portcheck::PortProbe::Unknown => portcheck::check_port(port).is_some(),
    };
    let occupant = listening.then(|| portcheck::identify_occupant(port));
    if let Some((entry, IdentityStatus::Matching)) = runtime_identity {
        let owned = occupant.as_ref().is_some_and(|occupant| {
            occupant
                .pid
                .is_some_and(|pid| portcheck::belongs_to_process_group(pid, entry.pgid))
        });
        let owner = occupant.as_ref().and_then(format_known_occupant);
        results.push(classify_port(service, port, true, listening, owner, owned));
        return;
    }

    results.push(classify_port(
        service,
        port,
        false,
        listening,
        occupant.as_ref().and_then(format_known_occupant),
        false,
    ));
}

fn format_known_occupant(occupant: &portcheck::PortOccupant) -> Option<String> {
    occupant.pid.map(|pid| format!("PID {pid}"))
}

fn classify_port(
    service: &str,
    port: u16,
    managed: bool,
    listening: bool,
    occupant: Option<String>,
    owned: bool,
) -> DoctorCheck {
    match (managed, listening, occupant, owned) {
        (true, true, Some(_), true) => DoctorCheck::ok(
            Some(service),
            format!("port {port} owned by managed service"),
        ),
        (true, true, Some(who), false) => DoctorCheck::fail(
            Some(service),
            format!("port {port} ownership"),
            format!("managed service expects the port, but it is held by {who}"),
        ),
        (true, true, None, _) => DoctorCheck::ok(
            Some(service),
            format!("port {port} listener present (owner unavailable)"),
        ),
        (true, false, _, _) => DoctorCheck::fail(
            Some(service),
            format!("port {port} listener"),
            "managed service is running but is not listening",
        ),
        (false, false, _, _) => DoctorCheck::ok(Some(service), format!("port {port} available")),
        (false, true, who, _) => DoctorCheck::fail(
            Some(service),
            format!("port {port} occupied by external process"),
            who.unwrap_or_else(|| "owner could not be identified".to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_diagnostics_distinguish_managed_external_and_missing_listener() {
        let managed = classify_port("api", 3000, true, true, Some("PID 10".to_string()), true);
        assert!(managed.ok);
        assert!(managed.label.contains("managed service"));

        let external = classify_port("api", 3000, false, true, Some("PID 11".to_string()), false);
        assert!(!external.ok);
        assert!(external.label.contains("external process"));

        let missing = classify_port("api", 3000, true, false, None, false);
        assert!(!missing.ok);
        assert!(missing.detail.unwrap().contains("not listening"));

        let unavailable_owner = classify_port("api", 3000, true, true, None, false);
        assert!(unavailable_owner.ok);
        assert!(unavailable_owner.label.contains("owner unavailable"));
    }

    #[test]
    fn compose_port_diagnostics_reject_host_listeners_that_shadow_docker() {
        let docker = portcheck::PortOccupant {
            pid: Some(10),
            process_name: Some("com.docker.backend".to_string()),
            command: Some("com.docker.backend".to_string()),
        };
        let host = portcheck::PortOccupant {
            pid: Some(11),
            process_name: Some("node".to_string()),
            command: Some("node backend.js".to_string()),
        };

        let healthy = classify_external_runtime_port(
            "identity",
            3001,
            portcheck::PortProbe::Listening,
            Some(true),
            std::slice::from_ref(&docker),
        );
        assert!(healthy.ok);

        let shadowed = classify_external_runtime_port(
            "identity",
            3001,
            portcheck::PortProbe::Listening,
            Some(true),
            &[docker, host],
        );
        assert!(!shadowed.ok);
        assert!(shadowed.label.contains("shadowed"));
        assert!(shadowed.detail.unwrap().contains("PID 11 (node)"));
    }
}
