//! Command-line definition.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use texrun_preview::{BackendChoice, MAX_DPI, PageRange};

use crate::duration::parse_timeout;

/// Default of `--timeout` (docs/security.md §3.1); a unit test checks that
/// it equals [`texrun_texlive::DEFAULT_TIMEOUT`].
pub const DEFAULT_TIMEOUT_ARG: &str = "60s";

/// Name of the default output directory, created next to the entrypoint.
pub const DEFAULT_OUTPUT_DIR_NAME: &str = "texrun-out";

const EXIT_CODES_HELP: &str = "\
Exit codes:
  0    the document compiled and a PDF was produced
  1    the document failed to compile (see the diagnostics), including when
       a resource limit stopped it (output size, CPU time, memory,
       processes: diagnostics of kind resource_limit)
  2    usage or input error: invalid arguments, entrypoint not found or
       outside --root, project rejected (symlink leaving the root, input
       limits exceeded, unsafe project root)
  3    texrun runtime error: latexmk missing or unusable, I/O errors,
       artifacts could not be copied to the output directory, --cgroup
       required without a usable cgroup
  4    the compile timed out (see --timeout)
  130  interrupted by SIGINT (Ctrl-C); 143 for SIGTERM, 129 for SIGHUP.
       A signal counts even if it arrives after the compile (e.g. while
       previews are rendered); otherwise previews never change the exit code.
With --json, the code is also in `texrun_exit_code`; the `exit` field is
how the latexmk process ended.";

const COMPILE_AFTER_HELP: &str = "\
Output:
  Without --json, the result (status, PDF path, main diagnostics) is printed
  on stdout; warnings and errors of texrun itself go to stderr. With --json,
  stdout contains exactly one JSON document, also on failure and on runtime
  errors, and stderr may still carry human-readable warnings.

  The project root (by default the entrypoint's directory) is copied into a
  temporary workspace, without VCS metadata, texrun-out/, .texrun/, an
  --output directory inside the project (unless it contains the entrypoint)
  and tool configuration such as latexmkrc (which texrun never reads). Files
  above the entrypoint's directory (e.g. \\input{../common/macros}) cannot
  be read by TeX; put the entrypoint in the project root instead.

  The PDF, the log and, after a successful compile, PNG previews of the
  first pages (preview/page-NNN.png) are copied to the output directory,
  replacing files of the same name. Symlinks inside the project are never
  followed on the way to the output directory (the default texrun-out/ is
  inside the project). Files from an earlier run (a PDF when the compile now
  fails, previews of pages no longer rendered) are not removed; rely on the
  exit code and the reported artifacts.
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

    #[allow(clippy::doc_markdown, reason = "the doc comment is the --help text")]
    /// Run the engine and the preview tools in cgroups of their own, with
    /// limits on their memory, processes and CPU use (Linux, cgroup v2):
    /// auto uses a delegated cgroup if there is one (e.g. under
    /// `systemd-run --user --scope -p Delegate=yes`) and otherwise relies on
    /// the per-process limits alone (recorded in resource_limits in the
    /// JSON); required fails (exit 3) instead; off never uses one.
    #[arg(long, value_enum, value_name = "MODE", default_value = "auto")]
    pub cgroup: CgroupMode,

    #[command(flatten)]
    pub preview: PreviewArgs,
}

/// Page preview options (#8). Previews are rendered only after a
/// successful compile; failures to render them are reported as notices and
/// never change the exit code.
#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Do not render page previews.
    #[arg(long, conflicts_with_all = ["pages", "preview_dpi", "preview_backend"])]
    pub no_preview: bool,

    /// Pages to render as PNG previews: N, N-M, N- or -M [default: the first
    /// 20 pages; at most 200 pages are rendered].
    #[arg(long, value_name = "RANGE", value_parser = parse_pages)]
    pub pages: Option<PageRange>,

    /// Preview resolution [default: 144]. Large pages are rendered at a lower
    /// resolution (long edge at most 4096 px).
    #[arg(long, value_name = "DPI", value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_DPI)))]
    pub preview_dpi: Option<u32>,

    #[allow(clippy::doc_markdown, reason = "the doc comment is the --help text")]
    /// Preview renderer [default: auto: MuPDF (mutool) if installed,
    /// otherwise Poppler (pdftoppm)].
    #[arg(long, value_enum, value_name = "BACKEND")]
    pub preview_backend: Option<PreviewBackend>,
}

/// `--cgroup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CgroupMode {
    Auto,
    Required,
    Off,
}

/// `--preview-backend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PreviewBackend {
    Auto,
    Mupdf,
    Poppler,
}

impl From<PreviewBackend> for BackendChoice {
    fn from(value: PreviewBackend) -> Self {
        match value {
            PreviewBackend::Auto => Self::Auto,
            PreviewBackend::Mupdf => Self::Mupdf,
            PreviewBackend::Poppler => Self::Poppler,
        }
    }
}

fn parse_pages(s: &str) -> Result<PageRange, String> {
    s.parse()
        .map_err(|e: texrun_preview::PageRangeError| e.to_string())
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
        assert_eq!(args.cgroup, CgroupMode::Auto);
        assert!(!args.preview.no_preview);
        assert_eq!(args.preview.pages, None);
    }

    #[test]
    fn preview_options() {
        let cli = Cli::try_parse_from([
            "texrun",
            "compile",
            "--pages",
            "2-4",
            "--preview-dpi",
            "72",
            "--preview-backend",
            "poppler",
            "m.tex",
        ])
        .unwrap();
        let Command::Compile(args) = cli.command;
        assert_eq!(
            args.preview.pages,
            Some(PageRange::new(2, Some(4)).unwrap())
        );
        assert_eq!(args.preview.preview_dpi, Some(72));
        assert_eq!(args.preview.preview_backend, Some(PreviewBackend::Poppler));
        for bad in [
            &["--pages", "0"][..],
            &["--pages", "3-1"],
            &["--preview-dpi", "0"],
            &["--preview-dpi", "5000"],
            &["--preview-backend", "ghostscript"],
            &["--no-preview", "--preview-dpi", "72"],
        ] {
            let mut command_line = vec!["texrun", "compile"];
            command_line.extend_from_slice(bad);
            command_line.push("m.tex");
            assert!(Cli::try_parse_from(&command_line).is_err(), "{bad:?}");
        }
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
