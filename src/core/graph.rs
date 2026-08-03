use std::collections::HashSet;

use anyhow::{bail, Result};

use crate::config::Config;

/// Resolve the full set of services to start for a template: the template's
/// own services plus all transitive dependencies.
pub fn services_for_template(config: &Config, template: &str) -> Result<Vec<String>> {
    let template_cfg = config
        .templates
        .get(template)
        .ok_or_else(|| anyhow::anyhow!("unknown template '{template}'"))?;
    resolve_start_order(config, &template_cfg.services)
}

/// RF-04/RF-05: given a set of requested services, return them plus their
/// transitive dependencies in a valid startup order (dependencies first).
/// Assumes the config has already passed `config::validate` (no cycles, no
/// dangling references) — this will still error defensively if not.
pub fn resolve_start_order(config: &Config, requested: &[String]) -> Result<Vec<String>> {
    let mut order = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();

    fn visit(
        name: &str,
        config: &Config,
        order: &mut Vec<String>,
        done: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if visiting.contains(name) {
            bail!("circular dependency detected while resolving '{name}'");
        }
        let service = config
            .services
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown service '{name}'"))?;

        visiting.insert(name.to_string());
        for dep in &service.depends_on {
            visit(dep, config, order, done, visiting)?;
        }
        visiting.remove(name);

        done.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }

    for name in requested {
        visit(name, config, &mut order, &mut done, &mut visiting)?;
    }

    Ok(order)
}

/// The reverse of the start order — used to stop a set of services without
/// stopping a service before its dependents.
pub fn stop_order(start_order: &[String]) -> Vec<String> {
    let mut v = start_order.to_vec();
    v.reverse();
    v
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{Config, ServiceConfig, TemplateConfig};

    use super::*;

    fn service(dependencies: &[&str]) -> ServiceConfig {
        ServiceConfig {
            command: Some("true".to_string()),
            depends_on: dependencies
                .iter()
                .map(|dependency| (*dependency).to_string())
                .collect(),
            ..ServiceConfig::default()
        }
    }

    #[test]
    fn resolves_a_dag_once_in_dependency_order_and_reverses_for_stop() {
        let config = Config {
            services: HashMap::from([
                ("database".to_string(), service(&[])),
                ("cache".to_string(), service(&[])),
                ("api".to_string(), service(&["database", "cache"])),
                ("worker".to_string(), service(&["database"])),
            ]),
            templates: HashMap::from([(
                "all".to_string(),
                TemplateConfig {
                    services: vec!["api".to_string(), "worker".to_string()],
                },
            )]),
            ..Config::default()
        };

        let start = services_for_template(&config, "all").unwrap();
        assert_eq!(start, ["database", "cache", "api", "worker"]);
        assert_eq!(stop_order(&start), ["worker", "api", "cache", "database"]);
    }

    #[test]
    fn rejects_unknown_nodes_and_cycles_defensively() {
        let mut config = Config {
            services: HashMap::from([("api".to_string(), service(&["database"]))]),
            ..Config::default()
        };

        let missing = resolve_start_order(&config, &["api".to_string()]).unwrap_err();
        assert!(missing.to_string().contains("unknown service 'database'"));

        config
            .services
            .insert("database".to_string(), service(&["api"]));
        let cycle = resolve_start_order(&config, &["api".to_string()]).unwrap_err();
        assert!(cycle.to_string().contains("circular dependency"));
    }
}
