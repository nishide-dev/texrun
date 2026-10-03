//! Command-line entry point for texrun.
//!
//! See `texrun --help` and docs/cli.md for the commands, the JSON document
//! and the exit codes.

mod cli;
mod compile;
mod duration;
mod escape;
mod gate;
mod human;
mod output;
mod report;
mod signals;

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;
use texrun_core::schema::Versioned;

use crate::cli::{Cli, Command};
use crate::report::{Category, CompileReport, ErrorInfo, Stage, exit, kind};

fn main() -> ExitCode {
    // The exec gate (hidden, internal): handled before clap, which must
    // never see the program's arguments.
    let mut args = std::env::args_os();
    if args.nth(1).is_some_and(|a| a == gate::SUBCOMMAND) {
        return texrun_process::run_gate(args);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return usage_error(&e),
    };
    match &cli.command {
        Command::Compile(args) => compile::main(args),
    }
}

/// Prints a clap error (or help / version). With `--json` on the command
/// line, a usage error is also reported as a JSON document on stdout.
fn usage_error(e: &clap::Error) -> ExitCode {
    let informational = matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if !informational && wants_json(std::env::args_os()) {
        let report = CompileReport {
            texrun_exit_code: Some(exit::USAGE),
            error: Some(ErrorInfo::new(
                Stage::Args,
                kind::USAGE,
                Category::Usage,
                e.render().to_string().trim_end(),
            )),
            ..CompileReport::default()
        };
        let mut stdout = io::stdout().lock();
        let _ = serde_json::to_writer_pretty(&mut stdout, &Versioned::new(&report))
            .map_err(io::Error::from)
            .and_then(|()| writeln!(stdout));
    }
    let _ = e.print();
    // clap: 0 for --help / --version, 2 (= exit::USAGE) otherwise.
    ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(exit::USAGE))
}

/// Whether `--json` appears among the options (before a `--`).
fn wants_json(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .skip(1)
        .take_while(|a| a != "--")
        .any(|a| a == "--json")
}

#[cfg(test)]
mod tests {
    use super::wants_json;

    #[test]
    fn json_flag_detection() {
        let args = |v: &[&str]| v.iter().map(Into::into).collect::<Vec<_>>();
        assert!(wants_json(args(&[
            "texrun", "compile", "--json", "--bogus"
        ])));
        assert!(!wants_json(args(&["texrun", "compile", "--", "--json"])));
        assert!(!wants_json(args(&["texrun", "compile", "--bogus"])));
    }
}
