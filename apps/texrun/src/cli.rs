//! Command-line definition.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::duration::parse_timeout;

/// Default of `--timeout` (docs/security.md §3.1); a unit test checks that
/// it equals [`texrun_texlive::DEFAULT_TIMEOUT`].
pub const DEFAULT_TIMEOUT_ARG: &str = "60s";

/// Name of the default output directory, created next to the entrypoint.
pub const DEFAULT_OUTPUT_DIR_NAME: &str = "texrun-out";

const EXIT_CODES_HELP: &str = "\
Exit codes:
  0    the document compiled and a PDF was produced
  1    the document failed to compile (see the diagnostics)
  2    usage or input error: invalid arguments, entrypoint not found or
       outside --root, project rejected (symlink leaving the root, input
       limits exceeded, unsafe project root)
  3    texrun runtime error: latexmk missing or unusable, I/O errors,
       artifacts could not be copied to the output directory
  4    the compile timed out (see --timeout)
  130  cancelled by SIGINT (Ctrl-C); 143 for SIGTERM, 129 for SIGHUP";

const COMPILE_AFTER_HELP: &str = "\
Output:
  Without --json, the result (status, PDF path, main diagnostics) is printed
  on stdout; warnings and errors of texrun itself go to stderr. With --json,
  stdout contains exactly one JSON document, also on failure and on runtime
  errors, and stderr may still carry human-readable warnings.

  The project root (by default the entrypoint's directory) is copied into a
  temporary workspace, without VCS metadata, texrun-out/, .texrun/ and tool
  configuration such as latexmkrc (which texrun never reads). Files above
  the entrypoint's directory (e.g. \\input{../common/macros}) cannot be read
  by TeX; put the entrypoint in the project root instead.

  The PDF and log are copied to the output directory, replacing files of the
  same name. A PDF from an earlier run is not removed when the compile
  fails; rely on the exit code or the JSON `outcome`.
";

/// Compile LaTeX documents and report structured results.
///
/// texrun runs TeX Live + latexmk in an isolated temporary workspace with a
/// timeout, and reports the outcome, structured diagnostics and the produced
/// PDF, as text or as JSON for tools and AI agents.
#[derive(Debug, Parser)]
#[command(
    name = "texrun",
    version = texrun_core::VERSION,
    about,
    long_about,
    arg_required_else_help = true,
    after_help = EXIT_CODES_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Compile a LaTeX document to PDF.
    #[command(after_help = format!("{EXIT_CODES_HELP}\n\n{COMPILE_AFTER_HELP}"))]
    Compile(CompileArgs),
}

/// Arguments of `texrun compile`.
#[derive(Debug, Args)]
pub struct CompileArgs {
    /// The main .tex file.
    #[arg(value_name = "ENTRYPOINT")]
    pub entrypoint: PathBuf,

    /// Print the result as one JSON document on stdout.
    #[arg(long)]
    pub json: bool,

    /// Directory to copy the PDF and log to [default: texrun-out/ next to
    /// the entrypoint].
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Project root copied into the workspace [default: the entrypoint's
    /// directory]. Must contain the entrypoint.
    #[arg(long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Wall-clock limit for the whole compile, e.g. 90s, 2m, 1500ms (a bare
    /// number is seconds).
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout, default_value = DEFAULT_TIMEOUT_ARG)]
    pub timeout: Duration,

    /// Keep the temporary workspace for debugging; its path is printed on
    /// stderr before the compile starts.
    #[arg(long)]
    pub keep_workspace: bool,

    #[allow(clippy::doc_markdown, reason = "the doc comment is the --help text")]
    /// Set SOURCE_DATE_EPOCH (and FORCE_SOURCE_DATE=1) for TeX, so the PDF
    /// dates are fixed for reproducible output. The environment variable of
    /// the same name is not passed through.
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(i64).range(0..))]
    pub source_date_epoch: Option<i64>,

    #[command(flatten)]
    pub preview: PreviewArgs,
}

/// Page preview options (#8), reserved until previews are integrated.
///
/// Hidden from the help for now. `--no-preview` is accepted and has no
/// effect (no previews are produced yet); `--pages` is rejected with a usage
/// error so that scripts do not silently get no previews.
#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Do not render page previews.
    #[arg(long, hide = true, conflicts_with = "pages")]
    pub no_preview: bool,

    /// Pages to render as previews: N, N-M, N- or -M.
    #[arg(long, hide = true, value_name = "RANGE")]
    pub pages: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn compile_defaults() {
        let cli = Cli::try_parse_from(["texrun", "compile", "main.tex"]).unwrap();
        let Command::Compile(args) = cli.command;
        assert_eq!(args.entrypoint, PathBuf::from("main.tex"));
        assert!(!args.json);
        assert_eq!(args.output, None);
        assert_eq!(args.root, None);
        assert_eq!(args.timeout, texrun_texlive::DEFAULT_TIMEOUT);
        assert!(!args.keep_workspace);
        assert_eq!(args.source_date_epoch, None);
    }

    #[test]
    fn rejects_zero_timeout_and_negative_epoch() {
        assert!(Cli::try_parse_from(["texrun", "compile", "--timeout", "0", "m.tex"]).is_err());
        assert!(
            Cli::try_parse_from(["texrun", "compile", "--source-date-epoch", "-1", "m.tex"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["texrun", "compile", "--no-preview", "--pages", "1", "m.tex"])
                .is_err()
        );
    }
}
