#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

mod cli;
mod commands;
mod files_cmd;
#[cfg(feature = "tui")]
mod tui;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use anvil::logger::Logger;
use cli::Cli;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // Help and version print to stdout and exit 0; usage errors are exit 2.
            let code = if err.use_stderr() { 2 } else { 0 };
            let _ = err.print();
            return ExitCode::from(code);
        }
    };
    // Bare `anvil` off a terminal: clap no longer prints the help itself.
    let Some(command) = cli::resolve(&cli) else {
        eprint!("{}", Cli::command().render_help());
        return ExitCode::from(2);
    };
    Logger::install(cli.verbose);
    log::debug!("{command:?}");
    match cli::run(&cli, command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            log::error!("{}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}
