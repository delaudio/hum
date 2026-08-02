mod cli;
mod config;
mod core;
mod doctor;
mod runtime;
mod tui;

use clap::Parser;

#[tokio::main]
async fn main() {
    let code = cli::run(cli::Cli::parse()).await;
    std::process::exit(code);
}
