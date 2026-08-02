use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::{self, RegistryError};
use crate::core::Manager;
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

#[derive(Debug, Parser)]
#[command(
    name = "hum",
    version,
    about = "Keep your local stack humming.",
    override_usage = "hum [OPTIONS] <PROJECT> <TEMPLATE> [COMMAND]"
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

    /// Registered project name, for example `compri`
    pub project: Option<String>,

    /// Template name, for example `all-services`
    pub template: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the selected template or listed services
    Start {
        services: Vec<String>,
        /// Transitional v1 behavior; removed when persistent runtime lands
        #[arg(long)]
        detach: bool,
    },
    /// Stop the selected template or listed services
    Stop { services: Vec<String> },
    /// Restart the selected template or listed services
    Restart { services: Vec<String> },
    /// Show status for services in the selected template
    Status,
    /// Show captured logs for a service
    Logs {
        service: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Check the selected local environment for common problems
    Doctor,
    /// Open the TUI in the selected project/template context
    Tui,
    /// Configuration-related utilities
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate registry, project configuration, and template selection
    Validate,
}

pub async fn run(cli: Cli) -> i32 {
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

        Command::Doctor => {
            let results = doctor::run_with_env(&loaded.config, &loaded.root_dir, &env_overrides);
            print_doctor(&results);
            if doctor::all_passed(&results) {
                EXIT_OK
            } else {
                EXIT_DOCTOR_FAILED
            }
        }

        Command::Tui => {
            let manager = Arc::new(Manager::with_env(loaded, env_overrides));
            match tui::run(manager, Some(template)).await {
                Ok(()) => EXIT_OK,
                Err(error) => {
                    eprintln!("✗ TUI error: {error}");
                    EXIT_GENERIC
                }
            }
        }

        Command::Start { services, detach } => {
            let manager = Arc::new(Manager::with_env(loaded, env_overrides));
            for service in &services {
                if !manager.config.services.contains_key(service) {
                    eprintln!("✗ unknown service '{service}'");
                    return EXIT_SERVICE_NOT_FOUND;
                }
            }

            let result = if services.is_empty() {
                manager.start_template(&template).await
            } else {
                manager.start_services(&services).await
            };

            let start_ok = match &result {
                Ok(started) => {
                    println!("✓ started: {}", started.join(", "));
                    true
                }
                Err(error) => {
                    eprintln!("✗ {error}");
                    false
                }
            };
            print_status(&manager);

            if detach {
                eprintln!(
                    "⚠ transitional --detach mode is unsupervised until runtime registry support lands"
                );
                return if start_ok { EXIT_OK } else { EXIT_START_FAILED };
            }
            if !start_ok {
                let _ = manager.stop_all().await;
                return EXIT_START_FAILED;
            }

            println!("\nhum is supervising these services in the foreground. Press Ctrl+C to stop them and exit.");
            wait_for_ctrl_c_and_shutdown(&manager).await;
            EXIT_OK
        }

        Command::Stop { services } => {
            let manager = Manager::with_env(loaded, env_overrides);
            let targets = match selected_services(&manager, &template, services) {
                Ok(targets) => targets,
                Err(code) => return code,
            };
            for name in &targets {
                if let Err(error) = manager.stop_service(name).await {
                    eprintln!("✗ failed to stop '{name}': {error}");
                    return EXIT_STOP_FAILED;
                }
            }
            println!("✓ stopped: {}", targets.join(", "));
            EXIT_OK
        }

        Command::Restart { services } => {
            let manager = Manager::with_env(loaded, env_overrides);
            let targets = match selected_services(&manager, &template, services) {
                Ok(targets) => targets,
                Err(code) => return code,
            };
            for name in &targets {
                if let Err(error) = manager.restart_service(name).await {
                    eprintln!("✗ failed to restart '{name}': {error}");
                    return EXIT_START_FAILED;
                }
            }
            println!("✓ restarted: {}", targets.join(", "));
            EXIT_OK
        }

        Command::Status => {
            let manager = Manager::with_env(loaded, env_overrides);
            print_status(&manager);
            EXIT_OK
        }

        Command::Logs {
            service,
            follow,
            lines,
        } => {
            if !loaded.config.services.contains_key(&service) {
                eprintln!("✗ unknown service '{service}'");
                return EXIT_SERVICE_NOT_FOUND;
            }
            println!(
                "(no persistent log store yet for '{service}'; runtime persistence is tracked separately)"
            );
            let _ = (follow, lines);
            EXIT_RUNTIME_INCOHERENT
        }
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
            "up" | "down" | "status" | "logs" | "restart" | "doctor" | "config"
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
        Err(error @ RegistryError::ReservedProject(_)) => {
            eprintln!("✗ {error}");
            Err(EXIT_INVALID_CONFIG)
        }
        Err(error) => {
            eprintln!("✗ {error}");
            Err(EXIT_INVALID_CONFIG)
        }
    }
}

fn selected_services(
    manager: &Manager,
    template: &str,
    requested: Vec<String>,
) -> Result<Vec<String>, i32> {
    if requested.is_empty() {
        return crate::core::graph::services_for_template(&manager.config, template).map_err(
            |error| {
                eprintln!("✗ {error}");
                EXIT_TEMPLATE_NOT_FOUND
            },
        );
    }
    for service in &requested {
        if !manager.config.services.contains_key(service) {
            eprintln!("✗ unknown service '{service}'");
            return Err(EXIT_SERVICE_NOT_FOUND);
        }
    }
    Ok(requested)
}

async fn wait_for_ctrl_c_and_shutdown(manager: &Arc<Manager>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => manager.reap_exited(),
        }
    }
    println!("\nstopping services...");
    let _ = manager.stop_all().await;
    println!("✓ all services stopped");
}

fn print_status(manager: &Manager) {
    println!(
        "{:<22} {:<10} {:<22} {:<10} DETAIL",
        "SERVICE", "PROCESS", "PORT", "HEALTH"
    );
    for view in manager.all_views() {
        let port = view
            .port
            .map(|port| format!("{port}/{}", view.port_state.label()))
            .unwrap_or_else(|| view.port_state.label().to_string());
        let detail = view.last_error.or(view.health_detail).unwrap_or_default();
        println!(
            "{:<22} {:<10} {:<22} {:<10} {}",
            view.name,
            view.process.label(),
            port,
            view.health.label(),
            detail
        );
    }
}

fn print_doctor(results: &[doctor::DoctorCheck]) {
    let mut current_scope: Option<&str> = None;
    for result in results {
        let scope = result.scope.as_deref();
        if scope != current_scope {
            if let Some(scope) = scope {
                println!("\n{scope}\n");
            } else if current_scope.is_some() {
                println!();
            }
            current_scope = scope;
        }
        if result.ok {
            println!("✓ {}", result.label);
        } else {
            println!(
                "✗ {}: {}",
                result.label,
                result.detail.as_deref().unwrap_or("")
            );
        }
    }
    println!();
    if doctor::all_passed(results) {
        println!("All checks passed.");
    } else {
        let failed = results.iter().filter(|result| !result.ok).count();
        println!("{failed} check(s) failed.");
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn parses_project_template_command_contract() {
        let cli = Cli::try_parse_from(["hum", "compri", "all-services", "start"]).unwrap();
        assert_eq!(cli.project.as_deref(), Some("compri"));
        assert_eq!(cli.template.as_deref(), Some("all-services"));
        assert!(matches!(
            cli.command,
            Some(Command::Start {
                services,
                detach: false
            }) if services.is_empty()
        ));
    }

    #[test]
    fn parses_service_scoped_logs() {
        let cli = Cli::try_parse_from([
            "hum",
            "compri",
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
            }) if service == "api"
        ));
    }

    #[test]
    fn parses_repeatable_environment_overrides() {
        let cli = Cli::try_parse_from([
            "hum",
            "compri",
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
    fn reports_missing_selection_parts_with_distinct_exit_codes() {
        let missing_project = Cli::try_parse_from(["hum"]).unwrap();
        assert_eq!(
            required_selection(&missing_project),
            Err(EXIT_PROJECT_NOT_FOUND)
        );

        let missing_template = Cli::try_parse_from(["hum", "compri"]).unwrap();
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
