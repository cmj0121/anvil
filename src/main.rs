#![forbid(unsafe_code)]

mod cli;
mod commands;
mod config;
mod document;
mod editor;
// The facts and transport modules are wired into `doctor` and `apply` by U7;
// those subcommands are stubs until then.
#[allow(dead_code)]
mod facts;
mod inventory;
mod logger;
mod paths;
mod secret;
mod ssh;
#[allow(dead_code)]
mod transport;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;
use logger::Logger;

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
    Logger::install(cli.verbose);
    log::debug!("{:?}", cli.command);
    match cli::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            log::error!("{}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}
