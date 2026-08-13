use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};

use crate::config::{self, RegistryError};
use crate::runtime::project::ProjectRuntime;
use crate::{doctor, tui};

pub const EXIT_OK: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
pub const EXIT_INVALID_CONFIG: i32 = 2;
pub const EXIT_PROJECT_NOT_FOUND: i32 = 3;
pub const EXIT_TEMPLATE_NOT_FOUND: i32 = 4;
pub const EXIT_SERVICE_NOT_FOUND: i32 = 5;
pub const EXIT_START_FAILED: i32 = 6;
pub const EXIT_STOP_FAILED: i32 = 7;
#[allow(dead_code)]
pub const EXIT_HEALTHCHECK_FAILED: i32 = 8;
pub const EXIT_DOCTOR_FAILED: i32 = 9;
pub const EXIT_RUNTIME_INCOHERENT: i32 = 10;
pub const EXIT_REGISTRY_IO: i32 = 11;

#[derive(Debug, Parser)]
#[command(
    name = "hum",
    version,
    about = "Keep your local stack humming.",
    override_usage = "hum [OPTIONS] <PROJECT> <TEMPLATE> [COMMAND]\n       hum [OPTIONS] project register <NAME> <CONFIG>",
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Path to the global project registry (default: ~/.config/hum/config.yaml)
    #[arg(long, global = true)]
    pub registry: Option<PathBuf>,

    /// Explicit project hum.yaml; bypasses the global registry
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Override a service environment value (repeatable, KEY=VALUE)
    #[arg(
        long = "env",
        global = true,
        value_name = "KEY=VALUE",
        action = clap::ArgAction::Append
    )]
    pub env: Vec<String>,

    /// Registered project name, for example `demo`
    pub project: Option<String>,

    /// Template name, for example `all-services`
    pub template: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage machine-local project registrations
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Start the selected template or listed services
    Start {
        services: Vec<String>,
        #[command(flatten)]
        exclusions: ExclusionArgs,
        /// Transitional v1 behavior; removed when persistent runtime lands
        #[arg(long)]
        detach: bool,
    },
    /// Stop the selected template or listed services
    Stop {
        services: Vec<String>,
        /// Grace period before SIGKILL
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        timeout: std::time::Duration,
    },
    /// Restart the selected template or listed services
    Restart {
        services: Vec<String>,
        /// Grace period before SIGKILL during the stop phase
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        timeout: std::time::Duration,
    },
    /// Stop the whole project and delete Compose-owned volumes
    Reset {
        /// Bypass the interactive project-name confirmation
        #[arg(long)]
        yes: bool,
        /// Grace period for local processes before Compose data is removed
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        timeout: std::time::Duration,
    },
    /// Show status for services in the selected template
    Status,
    /// Show the resolved service/task order without side effects
    Plan {
        services: Vec<String>,
        #[command(flatten)]
        exclusions: ExclusionArgs,
        #[arg(long)]
        json: bool,
    },
    /// Provider-backed environment utilities
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },
    /// Show captured logs for the template or one service
    Logs {
        service: Option<String>,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Check the selected local environment for common problems
    Doctor {
        #[command(flatten)]
        exclusions: ExclusionArgs,
    },
    /// Open the TUI in the selected project/template context
    Tui,
    /// Configuration-related utilities
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectAction {
    /// Register or update a project configuration from any checkout path
    Register {
        /// Project name used in `hum <PROJECT> ...`
        name: String,
        /// Path to the project's hum.yaml
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate registry, project configuration, and template selection
    Validate,
    /// Render a Compose runtime with service environment values redacted
    Compose {
        /// Compose runtime name; required only when the project defines more than one
        #[arg(long)]
        runtime: Option<String>,
        /// Output format
        #[arg(long, default_value = "yaml", value_parser = ["yaml", "json"])]
        format: String,
    },
}

#[derive(Debug, Clone, Default, Args)]
pub struct ExclusionArgs {
    /// Remove root services belonging to a template (repeatable)
    #[arg(long = "exclude", value_name = "TEMPLATE", action = clap::ArgAction::Append)]
    pub templates: Vec<String>,

    /// Strictly remove a service; fail if it remains a dependency (repeatable)
    #[arg(
        long = "exclude-service",
        value_name = "SERVICE",
        action = clap::ArgAction::Append
    )]
    pub excluded_services: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum SecretsAction {
    /// Fetch selected environment sources and refresh configured caches
    Sync {
        services: Vec<String>,
        #[command(flatten)]
        exclusions: ExclusionArgs,
    },
}

pub async fn run(cli: Cli) -> i32 {
    if let Some(Command::Project { action }) = &cli.command {
        if cli.project.is_some() || cli.template.is_some() {
            eprintln!("✗ project management commands do not take a project/template selection");
            return EXIT_INVALID_CONFIG;
        }
        return run_project_action(action, cli.registry.as_deref());
    }

    let env_overrides = match config::environment::parse_overrides(&cli.env) {
        Ok(overrides) => overrides,
        Err(error) => {
            eprintln!("✗ {error}");
            return EXIT_INVALID_CONFIG;
        }
    };
    let (project, template) = match required_selection(&cli) {
        Ok(selection) => (selection.0.to_string(), selection.1.to_string()),
        Err(code) => return code,
    };

    let loaded = match resolve_or_exit(&project, cli.config.as_deref(), cli.registry.as_deref()) {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };

    if !loaded.config.templates.contains_key(&template) {
        eprintln!("✗ unknown template '{template}' in project '{project}'");
        return EXIT_TEMPLATE_NOT_FOUND;
    }

    let command = cli.command.unwrap_or(Command::Tui);

    match command {
        Command::Config {
            action: ConfigAction::Validate,
        } => {
            println!("✓ configuration is valid");
            println!("  project:  {project}");
            println!("  template: {template}");
            println!("  base:     {}", loaded.base_path.display());
            if let Some(local) = &loaded.local_path {
                println!("  local:    {} (override applied)", local.display());
            }
            println!(
                "  {} service(s), {} template(s)",
                loaded.config.services.len(),
                loaded.config.templates.len()
            );
            EXIT_OK
        }

        Command::Config {
            action: ConfigAction::Compose { runtime, format },
        } => {
            let names = loaded
                .config
                .runtimes
                .iter()
                .filter(|(_, config)| {
                    matches!(config, crate::config::RuntimeConfig::Compose { .. })
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            let selected = match (runtime, names.as_slice()) {
                (Some(name), _) if names.contains(&name) => name,
                (Some(name), _) => {
                    eprintln!("✗ unknown Compose runtime '{name}'");
                    return EXIT_INVALID_CONFIG;
                }
                (None, [name]) => name.clone(),
                (None, []) => {
                    eprintln!("✗ project '{project}' has no Compose runtime");
                    return EXIT_INVALID_CONFIG;
                }
                (None, _) => {
                    eprintln!(
                        "✗ project '{project}' has multiple Compose runtimes; select one with --runtime"
                    );
                    return EXIT_INVALID_CONFIG;
                }
            };
            let compose = match crate::runtime::compose::ComposeRuntime::new(
                selected,
                project,
                loaded.config.clone(),
                loaded.root_dir.clone(),
                env_overrides,
            ) {
                Ok(compose) => compose,
                Err(error) => {
                    eprintln!("✗ failed to initialize Compose rendering: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            match compose.render_config_redacted(&format).await {
                Ok(output) => {
                    print!("{output}");
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("✗ Compose rendering failed: {error}");
                    EXIT_RUNTIME_INCOHERENT
                }
            }
        }

        Command::Doctor { exclusions } => {
            let selection =
                match resolve_command_selection(&loaded.config, &template, &[], &exclusions) {
                    Ok(selection) => selection,
                    Err(code) => return code,
                };
            print_selection_warnings(&selection.warnings);
            let runtime = match ProjectRuntime::new(project, loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ failed to initialize runtime diagnostics: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let order = selection.order;
            let results = match tokio::task::spawn_blocking(move || {
                doctor::run_with_project_selection(&runtime, &order)
            })
            .await
            {
                Ok(results) => results,
                Err(error) => {
                    eprintln!("✗ doctor task failed: {error}");
                    return EXIT_GENERIC;
                }
            };
            print_doctor(&results);
            if doctor::all_passed(&results) {
                EXIT_OK
            } else {
                EXIT_DOCTOR_FAILED
            }
        }

        Command::Tui => {
            let runtime = match ProjectRuntime::new(project, loaded, env_overrides) {
                Ok(runtime) => Arc::new(runtime),
                Err(error) => {
                    eprintln!("✗ failed to initialize runtime monitor: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            match tui::run(runtime, Some(template)).await {
                Ok(()) => EXIT_OK,
                Err(error) => {
                    eprintln!("✗ TUI error: {error}");
                    EXIT_GENERIC
                }
            }
        }

        Command::Start {
            services,
            exclusions,
            detach,
        } => {
            let selection = match resolve_command_selection(
                &loaded.config,
                &template,
                &services,
                &exclusions,
            ) {
                Ok(selection) => selection,
                Err(code) => return code,
            };
            print_selection_warnings(&selection.warnings);
            let runtime = match ProjectRuntime::new(project, loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ failed to initialize runtime: {error}");
                    return EXIT_START_FAILED;
                }
            };
            let result = runtime.start_selection(&selection.order).await;
            match result {
                Ok(report) => {
                    if !report.started.is_empty() {
                        println!("✓ started: {}", report.started.join(", "));
                    }
                    if !report.already_running.is_empty() {
                        println!("✓ already running: {}", report.already_running.join(", "));
                    }
                    if !report.reconciled.is_empty() {
                        println!("✓ reconciled: {}", report.reconciled.join(", "));
                    }
                    println!("  runtime: {}", runtime.registry().root().display());
                    if detach {
                        eprintln!("⚠ --detach is no longer needed; start is always detached");
                    }
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("✗ {error}");
                    EXIT_START_FAILED
                }
            }
        }

        Command::Stop { services, timeout } => {
            if let Err(code) = validate_services(&loaded.config, &services) {
                return code;
            }
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' template '{template}': {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let report = if services.is_empty() {
                runtime.stop_template(&template, timeout).await
            } else {
                runtime.stop_services(&services, timeout).await
            };
            match report {
                Ok(report) => print_stop_report(&project, &template, report),
                Err(error) => {
                    eprintln!(
                        "✗ project '{project}' template '{template}' stop failed: {error:#}\n  → inspect status and retry the failed service"
                    );
                    EXIT_STOP_FAILED
                }
            }
        }

        Command::Restart { services, timeout } => {
            if let Err(code) = validate_services(&loaded.config, &services) {
                return code;
            }
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' template '{template}': {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let report = if services.is_empty() {
                runtime.restart_template(&template, timeout).await
            } else {
                runtime.restart_services(&services, timeout).await
            };
            match report {
                Ok(report) => {
                    if !report.stop.succeeded() {
                        eprintln!(
                            "✗ project '{project}' template '{template}' restart aborted during stop; no services were started"
                        );
                        return print_stop_report(&project, &template, report.stop);
                    }
                    if !report.stop.stale_removed.is_empty() {
                        println!(
                            "✓ removed stale state before restart: {}",
                            report.stop.stale_removed.join(", ")
                        );
                    }
                    let start = report
                        .start
                        .expect("successful restart must include a start report");
                    let restarted = start.started;
                    if !restarted.is_empty() {
                        println!("✓ restarted: {}", restarted.join(", "));
                    }
                    if !start.already_running.is_empty() {
                        println!("✓ already running: {}", start.already_running.join(", "));
                    }
                    if !start.reconciled.is_empty() {
                        println!("✓ reconciled: {}", start.reconciled.join(", "));
                    }
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!(
                        "✗ project '{project}' template '{template}' restart failed: {error:#}\n  → inspect status, correct the failed service, then retry"
                    );
                    EXIT_START_FAILED
                }
            }
        }

        Command::Reset { yes, timeout } => {
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' reset initialization failed: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            if !runtime.has_compose_runtime() {
                eprintln!("✗ project '{project}' has no Compose runtime to reset");
                return EXIT_INVALID_CONFIG;
            }
            match confirm_reset(&project, yes) {
                Ok(false) => {
                    println!("Reset cancelled; no services or volumes were changed.");
                    return EXIT_OK;
                }
                Ok(true) => {}
                Err(error) => {
                    eprintln!("✗ {error}");
                    return EXIT_INVALID_CONFIG;
                }
            }
            match runtime.reset_all(timeout).await {
                Ok(report) if report.succeeded() => {
                    println!("✓ reset Compose data owned by project '{project}'");
                    EXIT_OK
                }
                Ok(report) => print_stop_report(&project, &template, report),
                Err(error) => {
                    eprintln!("✗ project '{project}' reset failed: {error:#}");
                    EXIT_STOP_FAILED
                }
            }
        }

        Command::Status => {
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' template '{template}': {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            match runtime.status_template(&template).await {
                Ok(statuses) => {
                    print_detached_status(&statuses);
                    if statuses.iter().any(|status| {
                        status
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.contains("identity mismatch"))
                    }) {
                        EXIT_RUNTIME_INCOHERENT
                    } else {
                        EXIT_OK
                    }
                }
                Err(error) => {
                    eprintln!(
                        "✗ project '{project}' template '{template}' status failed: {error:#}\n  → validate the runtime directory and retry"
                    );
                    EXIT_RUNTIME_INCOHERENT
                }
            }
        }

        Command::Plan {
            services,
            exclusions,
            json,
        } => {
            let selection = match resolve_command_selection(
                &loaded.config,
                &template,
                &services,
                &exclusions,
            ) {
                Ok(selection) => selection,
                Err(code) => return code,
            };
            print_selection_warnings(&selection.warnings);
            print_plan(&loaded.config, &template, &selection, &exclusions, json);
            EXIT_OK
        }

        Command::Secrets {
            action:
                SecretsAction::Sync {
                    services,
                    exclusions,
                },
        } => {
            let selection = match resolve_command_selection(
                &loaded.config,
                &template,
                &services,
                &exclusions,
            ) {
                Ok(selection) => selection,
                Err(code) => return code,
            };
            print_selection_warnings(&selection.warnings);
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' secret sync initialization failed: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let mut result = runtime.sync_selection_environment(&selection.order).await;
            let should_retry = match &result {
                Ok(report) => report.unavailable > 0,
                Err(error) => crate::config::environment::is_one_password_unavailable(error),
            };
            if should_retry
                && std::io::IsTerminal::is_terminal(&std::io::stdin())
                && std::env::var_os("OP_SERVICE_ACCOUNT_TOKEN").is_none()
            {
                match crate::config::environment::one_password_session_available() {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("1Password is not signed in; attempting interactive sign-in…");
                        match crate::config::environment::sign_in_one_password() {
                            Ok(true) => {
                                // Discard the failed pre-sign-in read before retrying.
                                // ProjectRuntime also starts every lifecycle action
                                // with a fresh cache; keeping this explicit protects
                                // the authentication boundary if that API changes.
                                crate::config::environment::clear_provider_read_cache();
                                result = runtime.sync_selection_environment(&selection.order).await;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                eprintln!("✗ interactive 1Password sign-in failed: {error}")
                            }
                        }
                    }
                    Err(error) => eprintln!("✗ cannot inspect 1Password session: {error}"),
                }
            }
            match result {
                Ok(report) => {
                    println!("✓ refreshed {} environment source(s)", report.refreshed);
                    if report.cached > 0 {
                        println!(
                            "  {} environment source(s) reused a valid private cache",
                            report.cached
                        );
                    }
                    if report.unavailable > 0 {
                        println!(
                            "  {} optional source(s) unavailable without a cache",
                            report.unavailable
                        );
                    }
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("✗ project '{project}' secret sync failed: {error}");
                    EXIT_START_FAILED
                }
            }
        }

        Command::Logs {
            service,
            follow,
            lines,
        } => {
            let runtime = match ProjectRuntime::new(project.clone(), loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ project '{project}' template '{template}': {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let services = match service {
                Some(service) if runtime.config().services.contains_key(&service) => vec![service],
                Some(service) => {
                    eprintln!("✗ unknown service '{service}'");
                    return EXIT_SERVICE_NOT_FOUND;
                }
                None => {
                    match crate::core::graph::services_for_template(runtime.config(), &template) {
                        Ok(services) => services,
                        Err(error) => {
                            eprintln!("✗ project '{project}' template '{template}': {error}");
                            return EXIT_TEMPLATE_NOT_FOUND;
                        }
                    }
                }
            };
            let mut persistent = Vec::new();
            let mut external = Vec::new();
            for service in services {
                match runtime.log_files(&service) {
                    Ok(Some((stdout, stderr))) => persistent.push(PersistentLogSource {
                        service,
                        stdout,
                        stderr,
                    }),
                    Ok(None) => external.push(service),
                    Err(error) => {
                        eprintln!("✗ project '{project}' template '{template}': {error}");
                        return EXIT_RUNTIME_INCOHERENT;
                    }
                }
            }
            let result = match (persistent.is_empty(), external.is_empty()) {
                (false, false) => tokio::try_join!(
                    show_persistent_logs(&persistent, &runtime.config().logs, lines, follow),
                    runtime.stream_external_logs(&external, lines, follow),
                )
                .map(|_| ()),
                (false, true) => {
                    show_persistent_logs(&persistent, &runtime.config().logs, lines, follow).await
                }
                (true, false) => runtime.stream_external_logs(&external, lines, follow).await,
                (true, true) => Ok(()),
            };
            match result {
                Ok(()) => EXIT_OK,
                Err(error) => {
                    eprintln!(
                        "✗ project '{project}' template '{template}' logs failed: {error:#}\n  → check permissions below {}",
                        runtime.registry().root().display()
                    );
                    EXIT_RUNTIME_INCOHERENT
                }
            }
        }
        Command::Project { .. } => unreachable!("project actions return before project resolution"),
    }
}

fn run_project_action(action: &ProjectAction, registry: Option<&Path>) -> i32 {
    match action {
        ProjectAction::Register { name, config } => {
            match config::register_project(name, config, registry) {
                Ok((registry, config)) => {
                    println!("✓ registered project '{name}'");
                    println!("  config:   {}", config.display());
                    println!("  registry: {}", registry.display());
                    EXIT_OK
                }
                Err(error @ RegistryError::ProjectMismatch { .. }) => {
                    eprintln!("✗ {error}");
                    EXIT_PROJECT_NOT_FOUND
                }
                Err(error @ RegistryError::ReservedProject(_))
                | Err(error @ RegistryError::InvalidProject(_)) => {
                    eprintln!("✗ {error}");
                    EXIT_INVALID_CONFIG
                }
                Err(
                    error @ (RegistryError::Io { .. }
                    | RegistryError::ConfigPath { .. }
                    | RegistryError::Write { .. }
                    | RegistryError::Serialize { .. }),
                ) => {
                    eprintln!("✗ {error}");
                    EXIT_REGISTRY_IO
                }
                Err(error) => {
                    eprintln!("✗ {error}");
                    EXIT_INVALID_CONFIG
                }
            }
        }
    }
}

fn confirm_reset(project: &str, yes: bool) -> anyhow::Result<bool> {
    use std::io::{IsTerminal, Write};

    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("reset requires interactive confirmation; use --yes for automation");
    }
    eprintln!(
        "This stops every project service and permanently deletes Compose volumes owned by '{project}'."
    );
    eprint!("Type {project} to continue: ");
    std::io::stderr().flush()?;
    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation)?;
    Ok(confirmation.trim() == project)
}

fn print_plan(
    config: &config::Config,
    template: &str,
    selection: &crate::core::graph::SelectionPlan,
    exclusions: &ExclusionArgs,
    json: bool,
) {
    let selected = selection
        .order
        .iter()
        .collect::<std::collections::HashSet<_>>();
    let mut reasons_by_unit = std::collections::HashMap::<String, Vec<String>>::new();
    for root in &selection.roots {
        reasons_by_unit
            .entry(root.clone())
            .or_default()
            .push("selected".to_string());
    }
    for dependent in &selection.order {
        let dependencies = config
            .services
            .get(dependent)
            .map(|service| service.depends_on.as_slice())
            .or_else(|| {
                config
                    .tasks
                    .get(dependent)
                    .map(|task| task.depends_on.as_slice())
            });
        for dependency in dependencies.into_iter().flatten() {
            if selected.contains(dependency) {
                reasons_by_unit
                    .entry(dependency.clone())
                    .or_default()
                    .push(format!("dependency of {dependent}"));
            }
        }
    }
    let units = selection
        .order
        .iter()
        .map(|name| {
            let reasons = reasons_by_unit
                .get(name)
                .cloned()
                .unwrap_or_else(|| vec!["transitive dependency".to_string()]);
            if let Some(task) = config.tasks.get(name) {
                serde_json::json!({
                    "name": name,
                    "kind": "task",
                    "depends_on": task.depends_on,
                    "reasons": reasons,
                })
            } else {
                let service = &config.services[name];
                serde_json::json!({
                    "name": name,
                    "kind": "service",
                    "runtime": service.runtime.as_deref().unwrap_or("process"),
                    "target": service.target,
                    "depends_on": service.depends_on,
                    "reasons": reasons,
                })
            }
        })
        .collect::<Vec<_>>();
    if json {
        let warnings = selection
            .warnings
            .iter()
            .map(|warning| {
                serde_json::json!({
                    "excluded_template": warning.excluded_template,
                    "service": warning.service,
                    "required_by": warning.required_by,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "template": template,
                "roots": selection.roots,
                "excluded_templates": exclusions.templates,
                "excluded_services": exclusions.excluded_services,
                "warnings": warnings,
                "units": units,
            }))
            .expect("plan serialization")
        );
        return;
    }
    println!("ORDER  KIND     NAME                     RUNTIME");
    for (index, unit) in units.iter().enumerate() {
        println!(
            "{:<6} {:<8} {:<24} {}",
            index + 1,
            unit["kind"].as_str().unwrap_or("-"),
            unit["name"].as_str().unwrap_or("-"),
            unit["runtime"].as_str().unwrap_or("-")
        );
    }
}

fn required_selection(cli: &Cli) -> Result<(&str, &str), i32> {
    let Some(project) = cli.project.as_deref() else {
        if cli.command.is_some() {
            eprintln!(
                "✗ command is missing project/template selection\n  → use: hum <PROJECT> <TEMPLATE> <COMMAND>"
            );
            return Err(EXIT_INVALID_CONFIG);
        }
        eprintln!("✗ missing project\n  → usage: hum <PROJECT> <TEMPLATE> <COMMAND>");
        return Err(EXIT_PROJECT_NOT_FOUND);
    };
    if cli.command.is_none()
        && matches!(
            project,
            "up" | "down"
                | "status"
                | "plan"
                | "secrets"
                | "logs"
                | "restart"
                | "reset"
                | "doctor"
                | "config"
        )
    {
        eprintln!(
            "✗ legacy v1 command syntax is not implicit in the project/template CLI\n  → use: hum <PROJECT> <TEMPLATE> <COMMAND>"
        );
        return Err(EXIT_INVALID_CONFIG);
    }
    let Some(template) = cli.template.as_deref() else {
        eprintln!("✗ missing template for project '{project}'\n  → usage: hum <PROJECT> <TEMPLATE> <COMMAND>");
        return Err(EXIT_TEMPLATE_NOT_FOUND);
    };
    Ok((project, template))
}

fn resolve_or_exit(
    project: &str,
    config: Option<&Path>,
    registry: Option<&Path>,
) -> Result<config::Loaded, i32> {
    match config::resolve_project(project, config, registry) {
        Ok(loaded) => Ok(loaded),
        Err(error @ RegistryError::UnknownProject(_))
        | Err(error @ RegistryError::ProjectMismatch { .. }) => {
            eprintln!("✗ {error}");
            Err(EXIT_PROJECT_NOT_FOUND)
        }
        Err(error @ RegistryError::ReservedProject(_))
        | Err(error @ RegistryError::InvalidProject(_)) => {
            eprintln!("✗ {error}");
            Err(EXIT_INVALID_CONFIG)
        }
        Err(error) => {
            eprintln!("✗ {error}");
            Err(EXIT_INVALID_CONFIG)
        }
    }
}

fn validate_services(config: &config::Config, requested: &[String]) -> Result<(), i32> {
    for service in requested {
        if !config.services.contains_key(service) {
            eprintln!("✗ unknown service '{service}'");
            return Err(EXIT_SERVICE_NOT_FOUND);
        }
    }
    Ok(())
}

fn resolve_command_selection(
    config: &config::Config,
    template: &str,
    requested: &[String],
    exclusions: &ExclusionArgs,
) -> Result<crate::core::graph::SelectionPlan, i32> {
    validate_services(config, requested)?;
    validate_services(config, &exclusions.excluded_services)?;
    for excluded in &exclusions.templates {
        if !config.templates.contains_key(excluded) {
            eprintln!("✗ unknown excluded template '{excluded}'");
            return Err(EXIT_TEMPLATE_NOT_FOUND);
        }
    }
    crate::core::graph::resolve_selection(
        config,
        template,
        requested,
        &exclusions.templates,
        &exclusions.excluded_services,
    )
    .map_err(|error| {
        eprintln!("✗ selection failed: {error}");
        EXIT_INVALID_CONFIG
    })
}

fn print_selection_warnings(warnings: &[crate::core::graph::SelectionWarning]) {
    for warning in warnings {
        eprintln!(
            "⚠ service '{}' was reintroduced from excluded template '{}' because '{}' depends on it",
            warning.service, warning.excluded_template, warning.required_by
        );
    }
}

fn print_stop_report(
    project: &str,
    template: &str,
    report: crate::runtime::detached::StopReport,
) -> i32 {
    if !report.stopped.is_empty() {
        println!("✓ stopped: {}", report.stopped.join(", "));
    }
    if !report.stale_removed.is_empty() {
        println!("✓ removed stale state: {}", report.stale_removed.join(", "));
    }
    if !report.already_stopped.is_empty() {
        println!("✓ already stopped: {}", report.already_stopped.join(", "));
    }
    if !report.blocked.is_empty() {
        eprintln!(
            "✗ not stopped because a dependent failed: {}",
            report.blocked.join(", ")
        );
    }
    for failure in &report.failures {
        eprintln!(
            "✗ project '{project}' template '{template}' failed to stop '{}': {}",
            failure.service, failure.detail
        );
    }
    if report.succeeded() {
        EXIT_OK
    } else {
        EXIT_STOP_FAILED
    }
}

fn print_detached_status(statuses: &[crate::runtime::detached::DetachedServiceStatus]) {
    println!(
        "{:<22} {:<10} {:<8} {:<22} {:<10} DETAIL",
        "SERVICE", "PROCESS", "PID", "PORT", "HEALTH"
    );
    for status in statuses {
        let port = status
            .configured_port
            .map(|port| format!("{port}/{}", status.port.label()))
            .unwrap_or_else(|| status.port.label().to_string());
        let pid = status
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<22} {:<10} {:<8} {:<22} {:<10} {}",
            status.name,
            status.process.label(),
            pid,
            port,
            status.health.label(),
            status.detail.as_deref().unwrap_or_default()
        );
    }
}

fn parse_duration(value: &str) -> Result<std::time::Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

async fn show_persistent_logs(
    sources: &[PersistentLogSource],
    config: &crate::config::LogConfig,
    lines: usize,
    follow: bool,
) -> anyhow::Result<()> {
    let redactor = crate::runtime::logs::Redactor::new(&config.redact_patterns)?;
    let multiple = sources.len() > 1;
    let mut any_output = false;
    let mut followers = Vec::new();
    for source in sources {
        let mut stdout = follow
            .then(|| {
                crate::runtime::logs::FileFollower::from_end_with_limit(
                    &source.stdout,
                    config.max_line_bytes,
                )
            })
            .transpose()?
            .flatten();
        let mut stderr = follow
            .then(|| {
                crate::runtime::logs::FileFollower::from_end_with_limit(
                    &source.stderr,
                    config.max_line_bytes,
                )
            })
            .transpose()?
            .flatten();
        let stdout_tail = match stdout.as_mut() {
            Some(follower) => follower.initial_tail(lines, config.rotated_files)?,
            None => {
                crate::runtime::logs::tail_history(&source.stdout, lines, config.rotated_files)?
            }
        };
        let stderr_tail = match stderr.as_mut() {
            Some(follower) => follower.initial_tail(lines, config.rotated_files)?,
            None => {
                crate::runtime::logs::tail_history(&source.stderr, lines, config.rotated_files)?
            }
        };
        any_output |= !stdout_tail.is_empty() || !stderr_tail.is_empty();
        for line in &stdout_tail {
            print_visible_log(
                multiple,
                &source.service,
                "stdout",
                &redactor.redact_bounded(line, config.max_line_bytes),
            );
        }
        for line in &stderr_tail {
            print_visible_log(
                multiple,
                &source.service,
                "stderr",
                &redactor.redact_bounded(line, config.max_line_bytes),
            );
        }
        if follow {
            followers.push(PersistentLogFollowers {
                service: source.service.clone(),
                stdout_path: source.stdout.clone(),
                stderr_path: source.stderr.clone(),
                stdout,
                stderr,
            });
        }
    }
    if !any_output && !follow {
        println!("(no log output yet)");
    }
    if !follow {
        return Ok(());
    }

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
            _ = ticker.tick() => {
                for source in &mut followers {
                    if source.stdout.is_none() {
                        source.stdout = crate::runtime::logs::FileFollower::from_start_with_limit(
                            &source.stdout_path,
                            config.max_line_bytes,
                        )?;
                    }
                    if source.stderr.is_none() {
                        source.stderr = crate::runtime::logs::FileFollower::from_start_with_limit(
                            &source.stderr_path,
                            config.max_line_bytes,
                        )?;
                    }
                    if let Some(follower) = source.stdout.as_mut() {
                        for line in follower.read_new_lines()? {
                            print_visible_log(
                                multiple,
                                &source.service,
                                "stdout",
                                &redactor.redact_bounded(&line, config.max_line_bytes),
                            );
                        }
                    }
                    if let Some(follower) = source.stderr.as_mut() {
                        for line in follower.read_new_lines()? {
                            print_visible_log(
                                multiple,
                                &source.service,
                                "stderr",
                                &redactor.redact_bounded(&line, config.max_line_bytes),
                            );
                        }
                    }
                }
            }
        }
    }
}

struct PersistentLogSource {
    service: String,
    stdout: PathBuf,
    stderr: PathBuf,
}

struct PersistentLogFollowers {
    service: String,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout: Option<crate::runtime::logs::FileFollower>,
    stderr: Option<crate::runtime::logs::FileFollower>,
}

fn print_visible_log(multiple: bool, service: &str, stream: &str, line: &str) {
    if multiple {
        println!("[{service}][{stream}] {line}");
    } else {
        println!("[{stream}] {line}");
    }
}

fn print_doctor(results: &[doctor::DoctorCheck]) {
    print!("{}", format_doctor(results));
}

fn format_doctor(results: &[doctor::DoctorCheck]) -> String {
    use std::collections::BTreeMap;
    use std::fmt::Write;

    const HEADERS: [&str; 4] = ["SERVICE", "STATUS", "CHECKS", "ISSUES"];
    let mut grouped: BTreeMap<String, (usize, usize, Vec<String>)> = BTreeMap::new();
    for result in results {
        let scope = result.scope.as_deref().unwrap_or("project").to_string();
        let summary = grouped.entry(scope).or_default();
        summary.1 += 1;
        if result.ok {
            summary.0 += 1;
        } else {
            let label = doctor_cell(&result.label);
            let detail = doctor_cell(result.detail.as_deref().unwrap_or(""));
            summary.2.push(if detail.is_empty() {
                label
            } else {
                format!("{label}: {detail}")
            });
        }
    }
    let mut rows = grouped
        .into_iter()
        .map(|(scope, (passed, total, issues))| {
            [
                scope,
                if issues.is_empty() {
                    "✓ PASS"
                } else {
                    "✗ FAIL"
                }
                .to_string(),
                format!("{passed}/{total}"),
                if issues.is_empty() {
                    "—".to_string()
                } else {
                    issues.join("; ")
                },
            ]
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        (left[0] != "project", &left[0]).cmp(&(right[0] != "project", &right[0]))
    });
    let widths: [usize; 4] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .chain(std::iter::once(HEADERS[column].len()))
            .max()
            .unwrap_or(0)
    });

    let mut output = String::new();
    writeln!(
        output,
        "{:<service_width$}  {:<status_width$}  {:<checks_width$}  {}",
        HEADERS[0],
        HEADERS[1],
        HEADERS[2],
        HEADERS[3],
        service_width = widths[0],
        status_width = widths[1],
        checks_width = widths[2],
    )
    .expect("writing to a string cannot fail");
    writeln!(
        output,
        "{:-<service_width$}  {:-<status_width$}  {:-<checks_width$}  {:-<issues_width$}",
        "",
        "",
        "",
        "",
        service_width = widths[0],
        status_width = widths[1],
        checks_width = widths[2],
        issues_width = widths[3],
    )
    .expect("writing to a string cannot fail");
    for row in &rows {
        writeln!(
            output,
            "{:<service_width$}  {:<status_width$}  {:<checks_width$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            service_width = widths[0],
            status_width = widths[1],
            checks_width = widths[2],
        )
        .expect("writing to a string cannot fail");
    }

    output.push('\n');
    if doctor::all_passed(results) {
        output.push_str("All checks passed.\n");
    } else {
        let failed = results.iter().filter(|result| !result.ok).count();
        writeln!(output, "{failed} check(s) failed.").expect("writing to a string cannot fail");
    }
    output
}

fn doctor_cell(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn doctor_output_is_a_compact_table_with_failure_details() {
        let output = format_doctor(&[
            doctor::DoctorCheck {
                scope: Some("api".to_string()),
                label: "node available".to_string(),
                ok: true,
                detail: None,
            },
            doctor::DoctorCheck {
                scope: Some("api".to_string()),
                label: "port 3000 occupied by external process".to_string(),
                ok: false,
                detail: Some("PID 42\nretry after stopping it".to_string()),
            },
            doctor::DoctorCheck {
                scope: None,
                label: "Configuration is valid".to_string(),
                ok: true,
                detail: None,
            },
        ]);

        let lines = output.lines().collect::<Vec<_>>();
        assert!(lines[0].contains("SERVICE"));
        assert!(lines[0].contains("STATUS"));
        assert!(lines[0].contains("CHECKS"));
        assert!(lines[0].contains("ISSUES"));
        assert_eq!(output.matches("\napi").count(), 1);
        assert!(output.contains("✗ FAIL"));
        assert!(output.contains("1/2"));
        assert!(output.contains("PID 42 retry after stopping it"));
        assert!(output.contains("project"));
        assert!(output.contains("1/1"));
        assert!(output.ends_with("1 check(s) failed.\n"));
    }

    #[test]
    fn parses_project_template_command_contract() {
        let cli = Cli::try_parse_from(["hum", "demo", "all-services", "start"]).unwrap();
        assert_eq!(cli.project.as_deref(), Some("demo"));
        assert_eq!(cli.template.as_deref(), Some("all-services"));
        assert!(matches!(
            cli.command,
            Some(Command::Start {
                services,
                detach: false,
                ..
            }) if services.is_empty()
        ));
    }

    #[test]
    fn parses_project_registration_without_stack_selection() {
        let cli = Cli::try_parse_from([
            "hum",
            "--registry",
            "/tmp/hum-registry.yaml",
            "project",
            "register",
            "demo",
            "./packs/demo/hum.yaml",
        ])
        .unwrap();
        assert!(cli.project.is_none());
        assert!(cli.template.is_none());
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                action: ProjectAction::Register { name, config }
            }) if name == "demo" && config == Path::new("./packs/demo/hum.yaml")
        ));
    }

    #[test]
    fn parses_service_scoped_logs() {
        let cli = Cli::try_parse_from([
            "hum",
            "demo",
            "all-services",
            "logs",
            "api",
            "--follow",
            "--lines",
            "25",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logs {
                service,
                follow: true,
                lines: 25
            }) if service.as_deref() == Some("api")
        ));
    }

    #[test]
    fn parses_template_wide_logs_without_a_service() {
        let cli =
            Cli::try_parse_from(["hum", "demo", "all-services", "logs", "--lines", "10"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logs {
                service: None,
                follow: false,
                lines: 10
            })
        ));
    }

    #[test]
    fn parses_redacted_compose_rendering() {
        let cli = Cli::try_parse_from([
            "hum",
            "demo",
            "all-services",
            "config",
            "compose",
            "--runtime",
            "infra",
            "--format",
            "json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                action: ConfigAction::Compose { runtime, format }
            }) if runtime.as_deref() == Some("infra") && format == "json"
        ));
    }

    #[test]
    fn parses_repeatable_environment_overrides() {
        let cli = Cli::try_parse_from([
            "hum",
            "demo",
            "all-services",
            "start",
            "--env",
            "API_URL=http://localhost:3000",
            "--env",
            "TOKEN=secret",
        ])
        .unwrap();
        assert_eq!(
            cli.env,
            [
                "API_URL=http://localhost:3000".to_string(),
                "TOKEN=secret".to_string()
            ]
        );
    }

    #[test]
    fn parses_plan_and_secret_sync_without_legacy_side_effects() {
        let plan = Cli::try_parse_from([
            "hum",
            "demo",
            "all-services",
            "plan",
            "api",
            "--json",
            "--exclude",
            "identity",
            "--exclude-service",
            "mail",
        ])
        .unwrap();
        assert!(matches!(
            plan.command,
            Some(Command::Plan {
                services,
                exclusions: ExclusionArgs {
                    templates,
                    excluded_services,
                },
                json: true,
            }) if services == ["api"] && templates == ["identity"] && excluded_services == ["mail"]
        ));

        let sync =
            Cli::try_parse_from(["hum", "demo", "all-services", "secrets", "sync", "api"]).unwrap();
        assert!(matches!(
            sync.command,
            Some(Command::Secrets {
                action: SecretsAction::Sync { services, .. }
            }) if services == ["api"]
        ));
    }

    #[test]
    fn reports_missing_selection_parts_with_distinct_exit_codes() {
        let missing_project = Cli::try_parse_from(["hum"]).unwrap();
        assert_eq!(
            required_selection(&missing_project),
            Err(EXIT_PROJECT_NOT_FOUND)
        );

        let missing_template = Cli::try_parse_from(["hum", "demo"]).unwrap();
        assert_eq!(
            required_selection(&missing_template),
            Err(EXIT_TEMPLATE_NOT_FOUND)
        );
    }

    #[test]
    fn help_shows_project_template_contract() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("<PROJECT> <TEMPLATE> [COMMAND]"));
        assert!(help.contains("start"));
        assert!(help.contains("status"));
    }

    #[test]
    fn rejects_legacy_v1_command_shape_with_migration_exit_code() {
        for args in [vec!["hum", "up", "frontend"], vec!["hum", "logs", "api"]] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(required_selection(&cli), Err(EXIT_INVALID_CONFIG));
        }
    }

    #[test]
    fn command_words_are_reserved_from_project_namespace() {
        let error = Cli::try_parse_from(["hum", "status", "all-services", "status"])
            .expect_err("status must be parsed as a command, not a project");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
