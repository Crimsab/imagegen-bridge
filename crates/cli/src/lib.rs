//! User-facing CLI over the same configuration, runtime, and HTTP transport as the Rust facade.

mod args;
mod commands;
mod dashboard;
mod doctor;
mod gateway;
mod output;
mod presentation;
mod setup;
mod update;

use clap::Parser as _;

/// Parses process arguments, runs one command, and returns a stable exit status.
#[must_use]
pub fn main_entry() -> std::process::ExitCode {
    let cli = args::Cli::parse();
    let passive_update_check = cli.allows_passive_update_check();
    let output = output::Output::new(
        cli.output_mode(),
        cli.quiet,
        cli.allow_inline,
        cli.local_artifact_paths,
    );
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
        Ok(()) => {
            if passive_update_check {
                runtime.block_on(update::notify_if_available(&output));
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            output.error(&error);
            std::process::ExitCode::from(output::exit_code(error.code))
        }
    }
}
