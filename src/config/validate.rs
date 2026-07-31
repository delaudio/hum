use std::collections::HashSet;
use std::path::Path;

use super::error::ConfigError;
use super::model::Config;

/// Validate a fully-merged configuration. Catches everything that can be
/// checked statically: version, dangling references and dependency cycles.
pub fn validate(config: &Config, file: &Path) -> Result<(), ConfigError> {
    if config.version != 1 {
        return Err(ConfigError::validation(
            file,
            "version",
            format!("unsupported config version {}", config.version),
            "set `version: 1` — this is the only supported version",
        ));
    }

    for (name, service) in &config.services {
        if service.command.is_none() {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}.command"),
                "service has no command to run",
                format!("add a `command:` to services.{name}"),
            ));
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

        for dep in &service.depends_on {
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

    for (name, profile) in &config.profiles {
        for svc in &profile.services {
            if !config.services.contains_key(svc) {
                return Err(ConfigError::validation(
                    file,
                    format!("profiles.{name}.services"),
                    format!("references unknown service '{svc}'"),
                    format!("define a `services.{svc}` entry, or fix the typo in profiles.{name}"),
                ));
            }
        }
    }

    detect_cycles(config, file)?;

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
