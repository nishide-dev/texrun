//! `texrun compile`.
//!
//! Pipeline: resolve the project → probe latexmk → create the workspace →
//! compile → (previews, #8) → collect artifacts to the output directory →
//! remove the workspace. Every step either adds to the [`CompileReport`] or
//! ends it with an [`ErrorInfo`]; the report is printed once at the end, and
//! the exit code is derived from it.
//!
//! Cleanup: the [`Workspace`] is owned by [`execute`] and dropped (or
//! explicitly closed) before it returns, on every path including errors and
//! cancellation. `main` returns an `ExitCode` instead of calling
//! `process::exit`, so no destructor is skipped.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use texrun_core::schema::Versioned;
use texrun_core::{CancelToken, CompileOptions, CompileResult, TypesetEngine};
use texrun_texlive::{LatexmkConfig, LatexmkEngine};
use texrun_workspace::{OverwritePolicy, ProjectInput, Workspace, WorkspaceConfig, WorkspaceError};

use crate::cli::{CompileArgs, DEFAULT_OUTPUT_DIR_NAME};
use crate::human::{self, Paths};
use crate::report::{
    Category, CompileReport, ErrorInfo, ProjectInfo, Stage, WorkspaceInfo, host_path,
};
use crate::signals::SignalGuard;

/// Runs `texrun compile` and prints its result.
pub fn main(args: &CompileArgs) -> ExitCode {
    let cancel = CancelToken::new();
    let mut report = CompileReport::default();
    let signals = match SignalGuard::install(cancel.clone()) {
        Ok(guard) => Some(guard),
        Err(e) => {
            report.error = Some(ErrorInfo::new(
                Stage::Setup,
                "signal_setup",
                Category::Runtime,
                format!("could not install signal handlers: {e}"),
            ));
            None
        }
    };
    if signals.is_some()
        && let Err(error) = execute(args, &cancel, &mut report)
    {
        report.error = Some(error);
    }
    print_report(args, &report);
    report.exit(signals.and_then(|s| s.received()))
}

fn print_report(args: &CompileArgs, report: &CompileReport) {
    if args.json {
        let mut stdout = io::stdout().lock();
        // A closed stdout (e.g. `| head`) must not turn into a panic; the
        // exit code still tells the outcome.
        let _ = serde_json::to_writer_pretty(&mut stdout, &Versioned::new(report))
            .map_err(io::Error::from)
            .and_then(|()| writeln!(stdout))
            .and_then(|()| stdout.flush());
        return;
    }
    if let Some(result) = &report.result {
        let mut stdout = io::stdout().lock();
        let _ = stdout
            .write_all(human::render_result(result, &display_paths(args), args.timeout).as_bytes())
            .and_then(|()| stdout.flush());
    }
    if let Some(error) = &report.error {
        eprint!("{}", human::render_error(error));
    }
}

/// Paths as the user would write them, for human-readable output.
fn display_paths(args: &CompileArgs) -> Paths {
    let entry_dir = args
        .entrypoint
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Paths {
        entrypoint: args.entrypoint.clone(),
        root: args.root.clone().unwrap_or_else(|| entry_dir.clone()),
        output: output_dir(args),
    }
}

/// `--output`, or `texrun-out/` next to the entrypoint.
fn output_dir(args: &CompileArgs) -> PathBuf {
    args.output.clone().unwrap_or_else(|| {
        args.entrypoint
            .parent()
            .unwrap_or(Path::new(""))
            .join(DEFAULT_OUTPUT_DIR_NAME)
    })
}

fn execute(
    args: &CompileArgs,
    cancel: &CancelToken,
    report: &mut CompileReport,
) -> Result<(), ErrorInfo> {
    check_args(args)?;

    let input = ProjectInput::from_host_entrypoint(&args.entrypoint, args.root.as_deref())
        .map_err(|e| ErrorInfo::from_workspace(Stage::Project, &e))?;
    report.project = Some(ProjectInfo {
        root: host_path(input.root()),
        entrypoint: input.entrypoint().to_string(),
    });
    check_root(input.root(), args.root.is_some())?;

    let output = output_dir(args);
    let config = workspace_config(args, input.root(), &output);

    let engine =
        LatexmkEngine::new(LatexmkConfig::default().with_source_date_epoch(args.source_date_epoch));
    // Reports a missing latexmk before anything is copied, and makes the
    // result carry the latexmk version.
    engine
        .probe()
        .map_err(|e| ErrorInfo::from_engine(Stage::Probe, &e))?;

    let options = CompileOptions::default().with_timeout(args.timeout);
    let ws = match Workspace::create(&input, options, &config) {
        Ok(ws) => ws,
        Err(e) => {
            if let WorkspaceError::KeptAfterFailure { path, .. } = &e {
                report.workspace = Some(WorkspaceInfo {
                    kept_path: Some(host_path(path)),
                    ..WorkspaceInfo::default()
                });
            }
            return Err(ErrorInfo::from_workspace(Stage::Workspace, &e));
        }
    };
    let mut info = WorkspaceInfo::from_report(ws.report());
    if ws.is_kept() {
        // Printed before the compile, so the location is known even if
        // texrun is killed.
        eprintln!(
            "texrun: keeping the workspace at {}",
            human::show(ws.path())
        );
        info.kept_path = Some(host_path(ws.path()));
    }
    eprint!("{}", human::render_workspace_warnings(ws.report()));
    report.workspace = Some(info);

    let ctx = ws.context().with_cancel(cancel.clone());
    // On error, `?` drops `ws`, which removes the workspace.
    let run = engine
        .run(&ctx, ws.request())
        .map_err(|e| ErrorInfo::from_engine(Stage::Compile, &e))?;
    let mut result = run.result;

    // #8: render page previews into `ws.output_dir()` here (unless
    // `--no-preview`) and attach them to `result`, so that the collection
    // below copies them with the PDF.

    let collected = collect(&ws, &mut result, &output);
    report.result = Some(result);
    if let Err(e) = ws.close() {
        eprintln!("warning: could not remove the workspace: {e}");
    }
    report.output_dir = collected?;
    Ok(())
}

/// Checks that are not expressed in the clap definition.
fn check_args(args: &CompileArgs) -> Result<(), ErrorInfo> {
    if args.preview.pages.is_some() {
        return Err(ErrorInfo::new(
            Stage::Args,
            "unsupported_option",
            Category::Usage,
            "--pages: page previews are not available in this version of texrun",
        ));
    }
    let paths = [
        ("the entrypoint", Some(&args.entrypoint)),
        ("--root", args.root.as_ref()),
        ("--output", args.output.as_ref()),
    ];
    for (what, path) in paths {
        if let Some(path) = path
            && path.to_str().is_none()
        {
            return Err(ErrorInfo::new(
                Stage::Args,
                "non_utf8_path",
                Category::Input,
                format!("{what} must be valid UTF-8: {}", human::show(path)),
            ));
        }
    }
    Ok(())
}

/// Refuses roots that would copy far more than a project: `/`, and — unless
/// given explicitly with `--root` — the home directory and the system
/// temporary directory (e.g. for `texrun compile ~/main.tex`).
fn check_root(root: &Path, explicit: bool) -> Result<(), ErrorInfo> {
    let canonical = |p: &Path| fs::canonicalize(p).ok();
    let is_home = std::env::var_os("HOME")
        .and_then(|h| canonical(Path::new(&h)))
        .is_some_and(|h| h == root);
    let is_temp = [std::env::temp_dir(), "/tmp".into(), "/var/tmp".into()]
        .iter()
        .filter_map(|p| canonical(p))
        .any(|t| t == root);
    let what = if root.parent().is_none() {
        "the filesystem root"
    } else if is_home {
        "your home directory"
    } else if is_temp {
        "a system temporary directory"
    } else {
        return Ok(());
    };
    let shown = human::show(root);
    if explicit && root.parent().is_some() {
        eprintln!(
            "warning: the project root {shown} is {what}; everything below it is copied into \
             the workspace"
        );
        return Ok(());
    }
    let error = ErrorInfo::new(
        Stage::Project,
        "unsafe_root",
        Category::Input,
        format!(
            "refusing to use {shown} ({what}) as the project root: everything below it would be \
             copied into the workspace"
        ),
    );
    Err(if root.parent().is_none() {
        error.with_hint("use a project directory as --root")
    } else {
        error.with_hint(
            "move the document into its own directory, or pass --root explicitly to confirm",
        )
    })
}

/// Workspace settings: `--keep-workspace`, and earlier outputs are not
/// copied back in: `texrun-out` anywhere, and the `--output` directory when
/// it is directly in the project root.
fn workspace_config(args: &CompileArgs, root: &Path, output: &Path) -> WorkspaceConfig {
    let mut config = WorkspaceConfig::default().with_keep(args.keep_workspace);
    config
        .excluded_names
        .push(DEFAULT_OUTPUT_DIR_NAME.to_owned());
    if let Some(name) = child_of(root, output) {
        config.excluded_root_names.push(name);
    }
    config
}

/// The name of `path` if it is (or would be) a direct child of `root`.
fn child_of(root: &Path, path: &Path) -> Option<String> {
    let resolved = fs::canonicalize(path).ok().or_else(|| {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        Some(fs::canonicalize(parent).ok()?.join(path.file_name()?))
    })?;
    if resolved.parent() == Some(root) {
        resolved.file_name()?.to_str().map(str::to_owned)
    } else {
        None
    }
}

/// Copies the artifacts to `output` and rewrites their paths relative to it.
/// Returns the absolute output directory, or `None` if there was nothing
/// to copy. On failure the artifacts are dropped from `result`: they only
/// existed in the workspace.
fn collect(
    ws: &Workspace,
    result: &mut CompileResult,
    output: &Path,
) -> Result<Option<String>, ErrorInfo> {
    if result.artifacts.is_empty() {
        return Ok(None);
    }
    let io_error = |what: &str, e: &io::Error| {
        ErrorInfo::new(
            Stage::Collect,
            "io",
            Category::Runtime,
            format!("{what} {}: {e}", human::show(output)),
        )
    };
    let prepared = fs::create_dir_all(output)
        .map_err(|e| io_error("could not create the output directory", &e))
        .and_then(|()| fs::canonicalize(output).map_err(|e| io_error("could not resolve", &e)));
    let dest = match prepared {
        Ok(dest) => dest,
        Err(e) => {
            result.artifacts.clear();
            return Err(e);
        }
    };
    match ws.collect_artifacts(&result.artifacts, &dest, OverwritePolicy::Replace) {
        Ok(collected) => {
            result.artifacts = collected;
            Ok(Some(host_path(&dest)))
        }
        Err(e) => {
            result.artifacts.clear();
            Err(ErrorInfo::from_workspace(Stage::Collect, &e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_root_is_always_refused() {
        let err = check_root(Path::new("/"), true).unwrap_err();
        assert_eq!(err.kind, "unsafe_root");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn temp_dir_is_refused_unless_explicit() {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(check_root(&temp, false).unwrap_err().kind, "unsafe_root");
        assert!(check_root(&temp, true).is_ok());
        let project = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(project.path()).unwrap();
        assert!(check_root(&project, false).is_ok());
    }

    #[test]
    fn output_directly_in_root_is_detected() {
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        assert_eq!(
            child_of(&root, &root.join("build")),
            Some("build".to_owned())
        );
        fs::create_dir(root.join("build")).unwrap();
        assert_eq!(
            child_of(&root, &root.join("build")),
            Some("build".to_owned())
        );
        assert_eq!(child_of(&root, &root.join("build/pdf")), None);
        assert_eq!(child_of(&root, &root), None);
    }
}
