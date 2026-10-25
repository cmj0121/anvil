#![forbid(unsafe_code)]

mod apply;
mod cli;
mod commands;
mod config;
mod doctor;
mod document;
mod editor;
mod facts;
mod inventory;
mod logger;
mod options;
mod packages;
mod paths;
mod secret;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the tty runner is wired into apply next")
)]
mod ssh;
mod term;
#[cfg(test)]
mod test_util;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the tty runner is wired into apply next")
)]
mod transport;
#[cfg(feature = "tui")]
mod tui;

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
