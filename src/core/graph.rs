use std::collections::HashSet;

use anyhow::{bail, Result};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionWarning {
    pub excluded_template: String,
    pub service: String,
    pub required_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPlan {
    pub order: Vec<String>,
    pub warnings: Vec<SelectionWarning>,
}

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
        let dependencies = config
            .services
            .get(name)
            .map(|service| service.depends_on.as_slice())
            .or_else(|| {
                config
                    .tasks
                    .get(name)
                    .map(|task| task.depends_on.as_slice())
            })
            .ok_or_else(|| anyhow::anyhow!("unknown service '{name}' or task"))?;

        visiting.insert(name.to_string());
        for dep in dependencies {
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

/// Resolve a command selection with subtractive template and service filters.
/// Template exclusions remove roots but allow required dependencies to return
/// with an explicit warning. Service exclusions are strict and fail if a
/// remaining unit still requires the excluded service.
pub fn resolve_selection(
    config: &Config,
    template: &str,
    requested: &[String],
    excluded_templates: &[String],
    excluded_services: &[String],
) -> Result<SelectionPlan> {
    let template_config = config
        .templates
        .get(template)
        .ok_or_else(|| anyhow::anyhow!("unknown template '{template}'"))?;
    let mut roots = if requested.is_empty() {
        template_config.services.clone()
    } else {
        requested.to_vec()
    };

    let mut seen_templates = HashSet::new();
    let mut excluded_by_template = Vec::new();
    for name in excluded_templates {
        if !seen_templates.insert(name) {
            bail!("template '{name}' is excluded more than once");
        }
        let excluded = config
            .templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown excluded template '{name}'"))?;
        for service in &excluded.services {
            excluded_by_template.push((service.clone(), name.clone()));
        }
    }

    let mut strict_exclusions = HashSet::new();
    for name in excluded_services {
        if !strict_exclusions.insert(name.clone()) {
            bail!("service '{name}' is excluded more than once");
        }
        if !config.services.contains_key(name) {
            bail!("unknown excluded service '{name}'");
        }
    }
    let template_exclusions = excluded_by_template
        .iter()
        .map(|(service, _)| service.as_str())
        .collect::<HashSet<_>>();
    roots.retain(|service| {
        !template_exclusions.contains(service.as_str()) && !strict_exclusions.contains(service)
    });

    let order = resolve_start_order(config, &roots)?;
    for excluded in excluded_services {
        if order.contains(excluded) {
            let required_by = direct_dependent(config, &order, excluded)
                .unwrap_or_else(|| "the remaining selection".to_string());
            bail!("cannot exclude service '{excluded}' because '{required_by}' depends on it");
        }
    }

    let ordered = order.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut warnings = Vec::new();
    let mut warned = HashSet::new();
    for (service, excluded_template) in excluded_by_template {
        if ordered.contains(service.as_str())
            && !strict_exclusions.contains(&service)
            && warned.insert((service.clone(), excluded_template.clone()))
        {
            warnings.push(SelectionWarning {
                required_by: direct_dependent(config, &order, &service)
                    .unwrap_or_else(|| "the remaining selection".to_string()),
                service,
                excluded_template,
            });
        }
    }

    Ok(SelectionPlan { order, warnings })
}

fn direct_dependent(config: &Config, order: &[String], dependency: &str) -> Option<String> {
    order.iter().find_map(|name| {
        unit_dependencies(config, name)
            .is_some_and(|dependencies| dependencies.iter().any(|item| item == dependency))
            .then(|| name.clone())
    })
}

fn unit_dependencies<'a>(config: &'a Config, name: &str) -> Option<&'a [String]> {
    config
        .services
        .get(name)
        .map(|service| service.depends_on.as_slice())
        .or_else(|| {
            config
                .tasks
                .get(name)
                .map(|task| task.depends_on.as_slice())
        })
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

    use crate::config::{Config, ServiceConfig, TaskConfig, TemplateConfig};

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

    #[test]
    fn includes_tasks_in_the_same_dependency_order() {
        let config = Config {
            tasks: HashMap::from([(
                "migrate".to_string(),
                TaskConfig {
                    command: vec!["migrate".to_string()],
                    check: None,
                    cwd: None,
                    env: HashMap::new(),
                    env_from: Vec::new(),
                    depends_on: vec!["database".to_string()],
                    timeout: std::time::Duration::from_secs(30),
                },
            )]),
            services: HashMap::from([
                ("database".to_string(), service(&[])),
                ("api".to_string(), service(&["migrate"])),
            ]),
            ..Config::default()
        };
        assert_eq!(
            resolve_start_order(&config, &["api".to_string()]).unwrap(),
            ["database", "migrate", "api"]
        );
    }

    #[test]
    fn template_exclusions_warn_when_a_dependency_reintroduces_a_service() {
        let config = Config {
            services: HashMap::from([
                ("database".to_string(), service(&[])),
                ("api".to_string(), service(&["database"])),
                ("mail".to_string(), service(&[])),
            ]),
            templates: HashMap::from([
                (
                    "all".to_string(),
                    TemplateConfig {
                        services: vec![
                            "api".to_string(),
                            "database".to_string(),
                            "mail".to_string(),
                        ],
                    },
                ),
                (
                    "infrastructure".to_string(),
                    TemplateConfig {
                        services: vec!["database".to_string(), "mail".to_string()],
                    },
                ),
            ]),
            ..Config::default()
        };

        let plan =
            resolve_selection(&config, "all", &[], &["infrastructure".to_string()], &[]).unwrap();
        assert_eq!(plan.order, ["database", "api"]);
        assert_eq!(
            plan.warnings,
            [SelectionWarning {
                excluded_template: "infrastructure".to_string(),
                service: "database".to_string(),
                required_by: "api".to_string(),
            }]
        );
    }

    #[test]
    fn strict_service_exclusions_block_required_dependencies() {
        let config = Config {
            services: HashMap::from([
                ("database".to_string(), service(&[])),
                ("api".to_string(), service(&["database"])),
            ]),
            templates: HashMap::from([(
                "all".to_string(),
                TemplateConfig {
                    services: vec!["api".to_string(), "database".to_string()],
                },
            )]),
            ..Config::default()
        };

        let error =
            resolve_selection(&config, "all", &[], &[], &["database".to_string()]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot exclude service 'database' because 'api' depends on it"
        );
    }
}
