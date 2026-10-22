#![forbid(unsafe_code)]

mod cli;
// The document, facts and transport modules are wired into the subcommands by
// U4 and U7; the subcommands are stubs until then.
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod document;
#[allow(dead_code)]
mod facts;
#[allow(dead_code)]
mod inventory;
mod logger;
#[allow(dead_code)]
mod paths;
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
