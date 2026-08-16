mod cli;
mod config;
mod core;
mod doctor;
mod runtime;
mod tui;

use clap::Parser;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    runtime::logs::exit_if_internal_sink(&args);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize async runtime");
    let code = runtime.block_on(cli::run(cli::Cli::parse_from(args)));
    std::process::exit(code);
}
