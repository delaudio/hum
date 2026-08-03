use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::loader::expand_home;
use crate::config::Config;
use crate::runtime::detached::DetachedRuntime;
use crate::runtime::portcheck;
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

/// RF-16: run every static/dynamic environment check the PRD describes.
/// `root_dir` is the directory the config file lives in, used to resolve
/// relative repository paths and requirement files.
pub fn run_with_runtime(runtime: &DetachedRuntime) -> Vec<DoctorCheck> {
    run_checks(
        runtime.config(),
        runtime.root_dir(),
        runtime.env_overrides(),
        runtime,
    )
}

fn run_checks(
    config: &Config,
    root_dir: &std::path::Path,
    cli_overrides: &HashMap<String, String>,
    runtime: &DetachedRuntime,
) -> Vec<DoctorCheck> {
    let mut results = Vec::new();

    // Repositories
    let mut repositories = config.repositories.iter().collect::<Vec<_>>();
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
        let (runtime_entry, registry_read_failed) = match runtime.registry().load(name) {
            Ok(entry) => (entry, false),
            Err(error) => {
                results.push(DoctorCheck::fail(
                    Some(name),
                    "Runtime registry",
                    error.to_string(),
                ));
                (None, true)
            }
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
            check_service_port(name, port, runtime_identity.as_ref(), &mut results);
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
}
