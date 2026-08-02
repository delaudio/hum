use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::error::ConfigError;
use super::model::{Config, HealthcheckConfig};

/// Validate a fully-merged configuration. Catches everything that can be
/// checked statically: version, dangling references and dependency cycles.
pub fn validate(config: &Config, file: &Path) -> Result<(), ConfigError> {
    if !matches!(config.version, 1 | 2) {
        return Err(ConfigError::validation(
            file,
            "version",
            format!("unsupported config version {}", config.version),
            "set `version: 1` for the legacy format or `version: 2` for project/templates",
        ));
    }

    if config.version == 2
        && config
            .project
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(ConfigError::validation(
            file,
            "project",
            "version 2 configuration requires a project identifier",
            "add `project: <name>` matching the global registry entry",
        ));
    }

    validate_names(config, file)?;

    // Services can be selected explicitly and templates may overlap, so a port
    // must identify at most one service across the whole project.
    let mut configured_ports: HashMap<u16, &str> = HashMap::new();
    for (name, service) in &config.services {
        if service
            .command
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}.command"),
                "service has no command to run",
                format!("add a `command:` to services.{name}"),
            ));
        }

        if service.port == Some(0) {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}.port"),
                "port must be between 1 and 65535",
                "choose the TCP port exposed by the service",
            ));
        }
        if let Some(port) = service.port {
            if let Some(other) = configured_ports.insert(port, name) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.port"),
                    format!("port {port} is also assigned to service '{other}'"),
                    "assign a distinct port to each service",
                ));
            }
        }

        if let Some(url) = &service.url {
            validate_url(file, format!("services.{name}.url"), url)?;
        }

        if let Some(healthcheck) = &service.healthcheck {
            validate_healthcheck(file, name, healthcheck)?;
        }

        if let Some(repo) = &service.repository {
            if !config.repositories.contains_key(repo) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.repository"),
                    format!("references unknown repository '{repo}'"),
                    format!(
                        "add `{repo}:` under `repositories:`, or fix the typo in services.{name}"
                    ),
                ));
            }
        }

        let mut dependencies = HashSet::new();
        for dep in &service.depends_on {
            if !dependencies.insert(dep) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    format!("dependency '{dep}' is listed more than once"),
                    "remove the duplicate dependency",
                ));
            }
            if dep == name {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    "a service cannot depend on itself",
                    format!("remove '{dep}' from services.{name}.depends_on"),
                ));
            }
            if !config.services.contains_key(dep) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    format!("depends on unknown service '{dep}'"),
                    format!("define a `services.{dep}` entry, or fix the typo in services.{name}"),
                ));
            }
        }
    }

    for (name, template) in &config.templates {
        let mut selected = HashSet::new();
        for svc in &template.services {
            if !selected.insert(svc) {
                return Err(ConfigError::validation(
                    file,
                    format!("templates.{name}.services"),
                    format!("service '{svc}' is listed more than once"),
                    "remove the duplicate service entry",
                ));
            }
            if !config.services.contains_key(svc) {
                return Err(ConfigError::validation(
                    file,
                    format!("templates.{name}.services"),
                    format!("references unknown service '{svc}'"),
                    format!("define a `services.{svc}` entry, or fix the typo in templates.{name}"),
                ));
            }
        }
    }

    detect_cycles(config, file)?;

    Ok(())
}

fn validate_names(config: &Config, file: &Path) -> Result<(), ConfigError> {
    for (namespace, names) in [
        (
            "repositories",
            config.repositories.keys().collect::<Vec<_>>(),
        ),
        ("services", config.services.keys().collect::<Vec<_>>()),
        ("templates", config.templates.keys().collect::<Vec<_>>()),
    ] {
        let mut normalized = HashMap::new();
        for name in names {
            if name.trim().is_empty() {
                return Err(ConfigError::validation(
                    file,
                    namespace,
                    "names cannot be empty",
                    "use a stable non-empty identifier",
                ));
            }
            let folded = name.to_lowercase();
            if let Some(other) = normalized.insert(folded, name) {
                return Err(ConfigError::validation(
                    file,
                    namespace,
                    format!("names '{other}' and '{name}' collide when case is ignored"),
                    "rename one entry so identifiers are unique across platforms",
                ));
            }
        }
    }
    Ok(())
}

fn validate_url(file: &Path, field: String, url: &str) -> Result<(), ConfigError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        ConfigError::validation(
            file,
            &field,
            format!("invalid URL: {error}"),
            "use an absolute http:// or https:// URL",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::validation(
            file,
            field,
            format!("unsupported URL scheme '{}'", parsed.scheme()),
            "use an http:// or https:// URL",
        ));
    }
    Ok(())
}

fn validate_healthcheck(
    file: &Path,
    service: &str,
    healthcheck: &HealthcheckConfig,
) -> Result<(), ConfigError> {
    let field = format!("services.{service}.healthcheck");
    match healthcheck {
        HealthcheckConfig::Http {
            url,
            timeout,
            interval,
            retries,
            expected_status,
        } => {
            validate_url(file, format!("{field}.url"), url)?;
            validate_probe_timing(file, &field, *timeout, *interval, *retries)?;
            if expected_status.is_empty()
                || expected_status
                    .iter()
                    .any(|status| !(100..=599).contains(status))
            {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.expected_status"),
                    "expected_status must contain valid HTTP status codes (100-599)",
                    "add at least one valid status code, for example 200",
                ));
            }
        }
        HealthcheckConfig::Tcp {
            host,
            port,
            timeout,
            interval,
            retries,
        } => {
            if host.trim().is_empty() {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.host"),
                    "TCP healthcheck host cannot be empty",
                    "set a host such as 127.0.0.1",
                ));
            }
            if *port == 0 {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.port"),
                    "TCP healthcheck port must be between 1 and 65535",
                    "set the port checked by the probe",
                ));
            }
            validate_probe_timing(file, &field, *timeout, *interval, *retries)?;
        }
    }
    Ok(())
}

fn validate_probe_timing(
    file: &Path,
    field: &str,
    timeout: std::time::Duration,
    interval: std::time::Duration,
    retries: u32,
) -> Result<(), ConfigError> {
    if timeout.is_zero() || interval.is_zero() || retries == 0 {
        return Err(ConfigError::validation(
            file,
            field,
            "healthcheck timeout, interval, and retries must be greater than zero",
            "configure non-zero probe timing and at least one retry",
        ));
    }
    Ok(())
}

/// RF-05: dependencies must form a DAG. Depth-first search with a
/// recursion stack to find back-edges (cycles).
fn detect_cycles(config: &Config, file: &Path) -> Result<(), ConfigError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit<'a>(
        name: &'a str,
        config: &'a Config,
        marks: &mut std::collections::HashMap<&'a str, Mark>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(name) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|s| *s == name).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(name.to_string());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(name, Mark::Visiting);
        stack.push(name);
        if let Some(service) = config.services.get(name) {
            for dep in &service.depends_on {
                if let Some(cycle) = visit(dep.as_str(), config, marks, stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        marks.insert(name, Mark::Done);
        None
    }

    let mut marks = std::collections::HashMap::new();
    let mut visited_roots: HashSet<&str> = HashSet::new();
    for name in config.services.keys() {
        if visited_roots.contains(name.as_str()) {
            continue;
        }
        let mut stack = Vec::new();
        if let Some(cycle) = visit(name.as_str(), config, &mut marks, &mut stack) {
            return Err(ConfigError::validation(
                file,
                "services.*.depends_on",
                format!("circular dependency detected: {}", cycle.join(" → ")),
                "break the cycle by removing one of the depends_on edges above",
            ));
        }
        visited_roots.insert(name.as_str());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{ServiceConfig, TemplateConfig};

    fn valid_config() -> Config {
        Config {
            version: 2,
            project: Some("demo".to_string()),
            services: HashMap::from([(
                "api".to_string(),
                ServiceConfig {
                    command: Some("cargo run".to_string()),
                    ..ServiceConfig::default()
                },
            )]),
            templates: HashMap::from([(
                "all".to_string(),
                TemplateConfig {
                    services: vec!["api".to_string()],
                },
            )]),
            ..Config::default()
        }
    }

    #[test]
    fn rejects_empty_command() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().command = Some("  ".to_string());
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn rejects_zero_port_and_invalid_url() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().port = Some(0);
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_config();
        config.services.get_mut("api").unwrap().url = Some("localhost:3000".to_string());
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn rejects_port_and_name_collisions() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().port = Some(3000);
        config.services.insert(
            "worker".to_string(),
            ServiceConfig {
                command: Some("cargo run".to_string()),
                port: Some(3000),
                ..ServiceConfig::default()
            },
        );
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_config();
        config.services.insert(
            "API".to_string(),
            ServiceConfig {
                command: Some("cargo run".to_string()),
                ..ServiceConfig::default()
            },
        );
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }
}
