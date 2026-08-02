use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::loader::expand_home;
use crate::config::Config;
use crate::runtime::portcheck;

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
pub fn run_with_env(
    config: &Config,
    root_dir: &std::path::Path,
    cli_overrides: &HashMap<String, String>,
) -> Vec<DoctorCheck> {
    let mut results = Vec::new();

    // Repositories
    for (name, repo) in &config.repositories {
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
    for (name, svc) in &config.services {
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
            match portcheck::check_port(port) {
                None => results.push(DoctorCheck::ok(
                    Some(name),
                    format!("port {port} available"),
                )),
                Some(occupant) => {
                    let who = occupant
                        .pid
                        .map(|pid| format!("PID {pid}"))
                        .unwrap_or_else(|| "unknown process".to_string());
                    results.push(DoctorCheck::fail(
                        Some(name),
                        format!("port {port} available"),
                        format!("already in use by {who}"),
                    ));
                }
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

pub fn all_passed(results: &[DoctorCheck]) -> bool {
    results.iter().all(|r| r.ok)
}
