//! User-facing CLI over the same configuration, runtime, and HTTP transport as the Rust facade.

mod args;
mod commands;
mod doctor;
mod output;
mod setup;

use clap::Parser as _;

/// Parses process arguments, runs one command, and returns a stable exit status.
#[must_use]
pub fn main_entry() -> std::process::ExitCode {
    let cli = args::Cli::parse();
    let output = output::Output::new(cli.output_mode(), cli.quiet, cli.allow_inline);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            output.internal_error(&format!("could not start async runtime: {error}"));
            return std::process::ExitCode::from(70);
        }
    };
    match runtime.block_on(commands::run(cli, &output)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            output.error(&error);
            std::process::ExitCode::from(output::exit_code(error.code))
        }
    }
}
