use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config;
use crate::core::Manager;
use crate::doctor;

pub const EXIT_OK: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
pub const EXIT_INVALID_CONFIG: i32 = 2;
pub const EXIT_SERVICE_NOT_FOUND: i32 = 3;
pub const EXIT_START_FAILED: i32 = 4;
#[allow(dead_code)]
pub const EXIT_HEALTHCHECK_FAILED: i32 = 5;
pub const EXIT_DOCTOR_FAILED: i32 = 6;

#[derive(Parser)]
#[command(name = "hum", version, about = "Keep your local stack humming.")]
pub struct Cli {
    /// Path to hum.yaml (default: discovered from the current directory upward)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start a profile or one or more services.
    ///
    /// Runs in the foreground and keeps services alive until Ctrl+C —
    /// `hum` has no daemon in the MVP (section 5/20 of the PRD), so
    /// services only stay supervised as long as this process is alive.
    Up {
        targets: Vec<String>,
        /// Start services and exit immediately instead of staying attached.
        /// The services become unsupervised: nothing will restart them on
        /// crash and no other `hum` invocation can stop them for you.
        #[arg(long)]
        detach: bool,
    },
    /// Stop every running service
    Down,
    /// Stop one or more services
    Stop { services: Vec<String> },
    /// Restart a service
    Restart { service: String },
    /// Show the status of every service
    Status,
    /// Show captured logs for a service
    Logs {
        service: String,
        /// Keep streaming new log lines
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Check the local environment for common problems
    Doctor,
    /// Configuration-related utilities
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Validate hum.yaml (and hum.local.yaml, if present)
    Validate,
}

/// Load config or print a readable error and return the right exit code.
fn load_or_exit(explicit: Option<&std::path::Path>) -> Result<config::Loaded, i32> {
    match config::load(explicit) {
        Ok(loaded) => Ok(loaded),
        Err(e) => {
            eprintln!("✗ {e}");
            Err(EXIT_INVALID_CONFIG)
        }
    }
}

pub async fn run(cli: Cli) -> i32 {
    let command = match cli.command {
        Some(c) => c,
        None => return EXIT_OK, // handled by caller (launches TUI)
    };

    let explicit = cli.config.as_deref();

    match command {
        Command::Config {
            action: ConfigAction::Validate,
        } => match load_or_exit(explicit) {
            Ok(loaded) => {
                println!("✓ configuration is valid");
                println!("  base:  {}", loaded.base_path.display());
                if let Some(local) = &loaded.local_path {
                    println!("  local: {} (override applied)", local.display());
                }
                println!(
                    "  {} service(s), {} profile(s)",
                    loaded.config.services.len(),
                    loaded.config.profiles.len()
                );
                EXIT_OK
            }
            Err(code) => code,
        },

        Command::Doctor => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let results = doctor::run(&loaded.config, &loaded.root_dir);
            print_doctor(&results);
            if doctor::all_passed(&results) {
                EXIT_OK
            } else {
                EXIT_DOCTOR_FAILED
            }
        }

        Command::Up { targets, detach } => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let manager = Arc::new(Manager::new(loaded));
            let result = if targets.len() == 1 && manager.config.profiles.contains_key(&targets[0])
            {
                manager.start_profile(&targets[0]).await
            } else if targets.is_empty() {
                eprintln!("✗ specify a profile or one or more service names");
                return EXIT_GENERIC;
            } else {
                for t in &targets {
                    if !manager.config.services.contains_key(t) {
                        eprintln!("✗ unknown service or profile '{t}'");
                        return EXIT_SERVICE_NOT_FOUND;
                    }
                }
                manager.start_services(&targets).await
            };

            let start_ok = match &result {
                Ok(started) => {
                    println!("✓ started: {}", started.join(", "));
                    true
                }
                Err(e) => {
                    eprintln!("✗ {e}");
                    false
                }
            };
            print_status(&manager);

            if detach {
                eprintln!(
                    "⚠ --detach: services are now unsupervised — `hum` will not restart them \
                     on crash, and no other `hum` invocation can stop them (no daemon in the MVP)."
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

        Command::Down => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let manager = Manager::new(loaded);
            match manager.stop_all().await {
                Ok(()) => {
                    println!("✓ all services stopped");
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("✗ {e}");
                    EXIT_GENERIC
                }
            }
        }

        Command::Stop { services } => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let manager = Manager::new(loaded);
            for name in &services {
                if !manager.config.services.contains_key(name) {
                    eprintln!("✗ unknown service '{name}'");
                    return EXIT_SERVICE_NOT_FOUND;
                }
            }
            for name in &services {
                if let Err(e) = manager.stop_service(name).await {
                    eprintln!("✗ failed to stop '{name}': {e}");
                    return EXIT_GENERIC;
                }
            }
            println!("✓ stopped: {}", services.join(", "));
            EXIT_OK
        }

        Command::Restart { service } => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let manager = Manager::new(loaded);
            if !manager.config.services.contains_key(&service) {
                eprintln!("✗ unknown service '{service}'");
                return EXIT_SERVICE_NOT_FOUND;
            }
            match manager.restart_service(&service).await {
                Ok(()) => {
                    println!("✓ restarted '{service}'");
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("✗ {e}");
                    EXIT_START_FAILED
                }
            }
        }

        Command::Status => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let manager = Manager::new(loaded);
            print_status(&manager);
            EXIT_OK
        }

        Command::Logs {
            service,
            follow,
            lines,
        } => {
            let loaded = match load_or_exit(explicit) {
                Ok(l) => l,
                Err(code) => return code,
            };
            if !loaded.config.services.contains_key(&service) {
                eprintln!("✗ unknown service '{service}'");
                return EXIT_SERVICE_NOT_FOUND;
            }
            // Note: without a running `hum` session to attach to, non-interactive
            // `logs` on a fresh process has nothing captured yet in the MVP
            // (log buffers live in the running session's memory, section 11.2/RF-12).
            println!(
                "(no running hum session found for '{service}' — start it with `hum up` first; \
                 use the TUI's log view, or `hum up` in the foreground, to see live output)"
            );
            let _ = (follow, lines);
            EXIT_OK
        }
    }
}

/// RF-07/RF-08 for the non-interactive path: keep the CLI process alive
/// (reaping crashed children — RNF-04) until Ctrl+C, then stop everything
/// gracefully before returning. Without this, services would keep running
/// unsupervised the moment the `up` command returned (violating section 5's
/// "must not stay alive after the main process exits").
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
        "{:<22} {:<10} {:<7} DETAIL",
        "SERVICE", "STATUS", "PORT"
    );
    for view in manager.all_views() {
        let port = view.port.map(|p| p.to_string()).unwrap_or_else(|| "—".into());
        let detail = view
            .blocked_reason
            .or(view.health_detail)
            .unwrap_or_default();
        println!(
            "{:<22} {:<10} {:<7} {}",
            view.name,
            view.status.label(),
            port,
            detail
        );
    }
}

fn print_doctor(results: &[doctor::DoctorCheck]) {
    let mut current_scope: Option<&str> = None;
    for r in results {
        let scope = r.scope.as_deref();
        if scope != current_scope {
            if let Some(s) = scope {
                println!("\n{s}\n");
            } else if current_scope.is_some() {
                println!();
            }
            current_scope = scope;
        }
        if r.ok {
            println!("✓ {}", r.label);
        } else {
            println!("✗ {}: {}", r.label, r.detail.as_deref().unwrap_or(""));
        }
    }
    println!();
    if doctor::all_passed(results) {
        println!("All checks passed.");
    } else {
        let failed = results.iter().filter(|r| !r.ok).count();
        println!("{failed} check(s) failed.");
    }
}
