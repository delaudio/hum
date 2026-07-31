mod cli;
mod config;
mod core;
mod doctor;
mod runtime;
mod tui;

use std::sync::Arc;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    if cli.command.is_none() {
        // Bare `hum`: load config and launch the TUI (section 8.1).
        match config::load(cli.config.as_deref()) {
            Ok(loaded) => {
                let manager = Arc::new(core::Manager::new(loaded));
                if let Err(e) = tui::run(manager).await {
                    eprintln!("✗ TUI error: {e}");
                    std::process::exit(cli::EXIT_GENERIC);
                }
                std::process::exit(cli::EXIT_OK);
            }
            Err(e) => {
                eprintln!("✗ {e}");
                std::process::exit(cli::EXIT_INVALID_CONFIG);
            }
        }
    }

    let code = cli::run(cli).await;
    std::process::exit(code);
}
