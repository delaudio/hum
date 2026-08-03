mod cli;
mod config;
mod core;
mod doctor;
mod runtime;
mod tui;

use clap::Parser;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    if let Some(code) = runtime::logs::try_run_internal_sink(&args) {
        std::process::exit(code);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize async runtime");
    let code = runtime.block_on(cli::run(cli::Cli::parse_from(args)));
    std::process::exit(code);
}
