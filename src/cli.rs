use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::config::{self, RegistryError};
use crate::runtime::detached::DetachedRuntime;
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
    /// Show status for services in the selected template
    Status,
    /// Show captured logs for the template or one service
    Logs {
        service: Option<String>,
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
            let runtime = match DetachedRuntime::new(project, loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ failed to initialize runtime diagnostics: {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            let results =
                match tokio::task::spawn_blocking(move || doctor::run_with_runtime(&runtime)).await
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
            let runtime = match DetachedRuntime::new(project, loaded, env_overrides) {
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

        Command::Start { services, detach } => {
            for service in &services {
                if !loaded.config.services.contains_key(service) {
                    eprintln!("✗ unknown service '{service}'");
                    return EXIT_SERVICE_NOT_FOUND;
                }
            }
            let runtime = match DetachedRuntime::new(project, loaded, env_overrides) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("✗ failed to initialize runtime: {error}");
                    return EXIT_START_FAILED;
                }
            };
            let result = if services.is_empty() {
                runtime.start_template(&template).await
            } else {
                runtime.start_services(&services).await
            };
            match result {
                Ok(report) => {
                    if !report.started.is_empty() {
                        println!("✓ started: {}", report.started.join(", "));
                    }
                    if !report.already_running.is_empty() {
                        println!("✓ already running: {}", report.already_running.join(", "));
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
            let runtime = match DetachedRuntime::new(project.clone(), loaded, env_overrides) {
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
            let runtime = match DetachedRuntime::new(project.clone(), loaded, env_overrides) {
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

        Command::Status => {
            let runtime = match DetachedRuntime::new(project.clone(), loaded, env_overrides) {
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

        Command::Logs {
            service,
            follow,
            lines,
        } => {
            let runtime = match DetachedRuntime::new(project.clone(), loaded, env_overrides) {
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
            let sources = services
                .into_iter()
                .map(|service| {
                    runtime
                        .log_paths(&service)
                        .map(|(stdout, stderr)| PersistentLogSource {
                            service,
                            stdout,
                            stderr,
                        })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            let sources = match sources {
                Ok(sources) => sources,
                Err(error) => {
                    eprintln!("✗ project '{project}' template '{template}': {error}");
                    return EXIT_RUNTIME_INCOHERENT;
                }
            };
            match show_persistent_logs(&sources, &runtime.config().logs, lines, follow).await {
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
            }) if service.as_deref() == Some("api")
        ));
    }

    #[test]
    fn parses_template_wide_logs_without_a_service() {
        let cli = Cli::try_parse_from(["hum", "compri", "all-services", "logs", "--lines", "10"])
            .unwrap();
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
