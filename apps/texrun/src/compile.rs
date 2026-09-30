//! `texrun compile`.
//!
//! Pipeline: resolve the project → check the output directory → probe
//! latexmk → create the workspace → compile → render page previews (after a
//! successful compile only) → copy artifacts to the output directory →
//! remove the workspace. Every step either adds to the [`CompileReport`] or
//! ends it with an [`ErrorInfo`]; the report is printed once at the end, and
//! the exit code is derived from it.
//!
//! Cleanup: the [`Workspace`] is owned by [`execute`] and dropped (or
//! explicitly closed) before it returns, on every path including errors and
//! cancellation. `main` returns an `ExitCode` instead of calling
//! `process::exit`, so no destructor is skipped.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use texrun_core::schema::Versioned;
use texrun_core::{
    Artifact, CancelToken, CompileOptions, CompileOutcome, CompileResult, DiagnosticKind, Severity,
    TypesetEngine, WorkspacePath,
};
use texrun_preview::{PreviewOptions, PreviewReport, Previewer};
use texrun_texlive::{LatexmkConfig, LatexmkEngine};
use texrun_workspace::{
    OverwritePolicy, ProjectInput, Workspace, WorkspaceConfig, WorkspaceError, WorkspaceErrorKind,
};

use crate::cli::{CompileArgs, DEFAULT_OUTPUT_DIR_NAME};
use crate::human::{self, Paths};
use crate::output::{self, OutputDirError};
use crate::report::{
    Category, CompileReport, ErrorInfo, Note, ProjectInfo, Stage, WorkspaceInfo, host_path, kind,
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
                kind::SIGNAL_SETUP,
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
    let code = report.exit_code(signals.and_then(|s| s.received()));
    report.texrun_exit_code = Some(code);
    print_report(args, &report);
    ExitCode::from(code)
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
        let text = human::render_result(report, result, &display_paths(args), args.timeout);
        let mut stdout = io::stdout().lock();
        let _ = stdout
            .write_all(text.as_bytes())
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
    let preview_options = preview_options(args, cancel)?;

    let input = ProjectInput::from_host_entrypoint(&args.entrypoint, args.root.as_deref())
        .map_err(|e| project_error(args, &e))?;
    report.project = Some(ProjectInfo {
        root: host_path(input.root()),
        entrypoint: input.entrypoint().to_string(),
    });
    if let Some(note) = check_root(input.root(), args.root.is_some())? {
        report.notes.push(note);
    }

    // Refuse an unusable output directory before compiling. It is checked
    // (and created) again when the artifacts are copied.
    let output = output_dir(args);
    let existing_output =
        output::walk(&output, input.root(), false).map_err(|e| output_error(&output, e))?;
    let (config, note) = workspace_config(args, &input, existing_output.as_deref());
    report.notes.extend(note);

    let engine =
        LatexmkEngine::new(LatexmkConfig::default().with_source_date_epoch(args.source_date_epoch));
    // Reports a missing latexmk before anything is copied, and makes the
    // result carry the latexmk version.
    engine
        .probe()
        .map_err(|e| ErrorInfo::from_engine(Stage::Probe, &e))?;
    if cancel.is_cancelled() {
        // Interrupted during the probe: do not copy the project at all.
        report.result = Some(CompileResult::new(
            CompileOutcome::Cancelled,
            engine.info(),
            Duration::ZERO,
        ));
        return Ok(());
    }

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

    // The engine does not start latexmk when cancellation was requested
    // while the project was being copied.
    let ctx = ws.context().with_cancel(cancel.clone());
    // On error, `?` drops `ws`, which removes the workspace.
    let run = engine
        .run(&ctx, ws.request())
        .map_err(|e| ErrorInfo::from_engine(Stage::Compile, &e))?;
    let mut result = run.result;
    if let Some(note) = parent_directory_note(&result) {
        report.notes.push(note);
    }

    // Previews are rendered into the workspace output directory and
    // attached to the result, so that the copy below includes them.
    let mut preview = render_previews(&ws, &result, preview_options, cancel);
    if let Some(preview) = &preview {
        preview.attach_to(&mut result);
    }

    let copied = copy_artifacts(&ws, &result.artifacts, &output, input.root());
    result.artifacts = copied.copied;
    if let Some(preview) = &mut preview {
        // Only list the images that reached the output directory.
        let paths: HashSet<_> = result.artifacts.iter().map(|a| &a.path).collect();
        preview.pages.retain(|p| paths.contains(&p.artifact.path));
    }
    report.artifacts_not_copied = copied.not_copied;
    report.output_dir = copied.dir.as_deref().map(host_path);
    report.result = Some(result);
    report.preview = preview;
    if let Err(e) = ws.close() {
        eprintln!("warning: could not remove the workspace: {e}");
    }
    copied.error.map_or(Ok(()), Err)
}

/// A note that is also printed on stderr right away.
fn warn_now(kind: &'static str, message: String) -> Note {
    eprintln!("warning: {}", crate::escape::escape(&message));
    Note {
        severity: Severity::Warning,
        kind,
        message,
        printed: true,
    }
}

/// Advice when TeX could not find a file above the entrypoint's directory,
/// which `--root` cannot fix (docs/security.md §3.5).
fn parent_directory_note(result: &CompileResult) -> Option<Note> {
    result
        .diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::MissingFile && d.message.contains("../"))
        .then(|| Note {
            severity: Severity::Info,
            kind: "parent_directory_input",
            message: "TeX cannot read files above the entrypoint's directory, even with --root; \
                      move the entrypoint into the directory that contains the files it includes"
                .to_owned(),
            printed: false,
        })
}

/// A project error, with a clearer hint when the entrypoint is a symlink
/// that leads outside the root.
fn project_error(args: &CompileArgs, e: &WorkspaceError) -> ErrorInfo {
    let info = ErrorInfo::from_workspace(Stage::Project, e);
    let is_link = fs::symlink_metadata(&args.entrypoint).is_ok_and(|m| m.file_type().is_symlink());
    if e.kind() == WorkspaceErrorKind::EntrypointOutsideRoot && is_link {
        let target = fs::canonicalize(&args.entrypoint)
            .map_or_else(|_| "an unresolvable path".to_owned(), |t| human::show(&t));
        return info.with_hint(format!(
            "the entrypoint is a symlink to {target}, which is outside the project root; pass \
             the real file, or a --root that contains it"
        ));
    }
    info
}

fn output_error(output: &Path, e: OutputDirError) -> ErrorInfo {
    match e {
        OutputDirError::Symlink(at) => ErrorInfo::new(
            Stage::Output,
            kind::UNSAFE_OUTPUT_PATH,
            Category::Runtime,
            format!(
                "refusing to use the output directory {}: {} is a symlink inside the project",
                human::show(output),
                human::show(&at)
            ),
        )
        .with_hint(
            "texrun does not follow symlinks inside the project to its output directory; \
             remove the symlink, or pass --output with a directory outside the project",
        ),
        OutputDirError::NotDirectory(at) => ErrorInfo::new(
            Stage::Output,
            kind::UNSAFE_OUTPUT_PATH,
            Category::Runtime,
            format!(
                "cannot use the output directory {}: {} exists and is not a directory",
                human::show(output),
                human::show(&at)
            ),
        )
        .with_hint("pass --output with a directory path (it is created if missing)"),
        OutputDirError::Io(what, at, e) => ErrorInfo::new(
            Stage::Output,
            kind::IO,
            Category::Runtime,
            format!("I/O error while {what} {}: {e}", human::show(&at)),
        ),
    }
}

/// The preview options from the command line, or `None` for
/// `--no-preview`. Validated before anything runs.
fn preview_options(
    args: &CompileArgs,
    cancel: &CancelToken,
) -> Result<Option<PreviewOptions>, ErrorInfo> {
    let p = &args.preview;
    if p.no_preview {
        return Ok(None);
    }
    // The same token as the compile: Ctrl-C also stops the renderer.
    let mut options = PreviewOptions::default().with_cancel(cancel.clone());
    if let Some(pages) = p.pages {
        options = options.with_pages(pages);
    }
    if let Some(dpi) = p.preview_dpi {
        options = options.with_dpi(dpi);
    }
    if let Some(backend) = p.preview_backend {
        options = options.with_backend(backend.into());
    }
    options.validate().map_err(|e| {
        ErrorInfo::new(
            Stage::Args,
            kind::INVALID_PREVIEW_OPTIONS,
            Category::Usage,
            e.to_string(),
        )
    })?;
    Ok(Some(options))
}

/// Renders previews of the PDF after a successful compile. Problems while
/// rendering are notices in the returned report, never errors.
fn render_previews(
    ws: &Workspace,
    result: &CompileResult,
    options: Option<PreviewOptions>,
    cancel: &CancelToken,
) -> Option<PreviewReport> {
    let options = options?;
    if !result.is_success() || cancel.is_cancelled() {
        return None;
    }
    let pdf = result.pdf()?;
    let output_root = ws.output_dir();
    let pdf = output_root.join(pdf.path.as_path());
    // Options were validated up front; `render` cannot fail otherwise.
    Previewer::detect()
        .with_exec_gate(crate::gate::exec_gate())
        .render(&pdf, &output_root, &options)
        .ok()
}

/// Checks that are not expressed in the clap definition.
fn check_args(args: &CompileArgs) -> Result<(), ErrorInfo> {
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
                kind::NON_UTF8_PATH,
                Category::Input,
                format!("{what} must be valid UTF-8: {}", human::show(path)),
            ));
        }
    }
    Ok(())
}

/// Refuses roots that would copy far more than a project: `/`, and — unless
/// given explicitly with `--root` — the home directory and the system
/// temporary directory (e.g. for `texrun compile ~/main.tex`). An explicit
/// one is accepted with a warning note.
fn check_root(root: &Path, explicit: bool) -> Result<Option<Note>, ErrorInfo> {
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
        return Ok(None);
    };
    let shown = human::show(root);
    if explicit && root.parent().is_some() {
        return Ok(Some(warn_now(
            "broad_project_root",
            format!(
                "the project root {shown} is {what}; everything below it is copied into the \
                 workspace"
            ),
        )));
    }
    let error = ErrorInfo::new(
        Stage::Project,
        kind::UNSAFE_ROOT,
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
/// copied back in: `texrun-out` anywhere, and the output directory
/// (`existing_output`, its canonical path if it already exists) at any depth
/// when it is inside the project root. When the output directory cannot be
/// left out because it contains the entrypoint, an info note says so.
fn workspace_config(
    args: &CompileArgs,
    input: &ProjectInput,
    existing_output: Option<&Path>,
) -> (WorkspaceConfig, Option<Note>) {
    let mut config = WorkspaceConfig::default().with_keep(args.keep_workspace);
    config
        .excluded_names
        .push(DEFAULT_OUTPUT_DIR_NAME.to_owned());
    let Some(dir) = existing_output else {
        return (config, None);
    };
    let entrypoints = entrypoint_paths(input);
    let note = match output_exclusion(dir, input.root(), &entrypoints) {
        OutputExclusion::Exclude(rel) => {
            config.excluded_paths.push(rel);
            None
        }
        OutputExclusion::ContainsEntrypoint => Some(Note {
            severity: Severity::Info,
            kind: "output_contains_entrypoint",
            message: format!(
                "the output directory {} contains the entrypoint, so it is copied into the \
                 workspace with the project, including the output of earlier runs; use an \
                 output directory that does not contain the document to keep them out",
                human::show(&output_dir(args))
            ),
            printed: false,
        }),
        OutputExclusion::NotInProject => None,
    };
    (config, note)
}

/// The entrypoint relative to the project root as given and, if it differs,
/// as resolved (the entrypoint or one of its directories may be a symlink
/// to somewhere else inside the root; the workspace layer follows it).
fn entrypoint_paths(input: &ProjectInput) -> Vec<WorkspacePath> {
    let given = input.entrypoint().clone();
    let resolved = fs::canonicalize(input.root().join(given.as_path()))
        .ok()
        .and_then(|real| WorkspacePath::from_path(real.strip_prefix(input.root()).ok()?).ok());
    match resolved {
        Some(real) if real != given => vec![given, real],
        _ => vec![given],
    }
}

/// What to do with the output directory when creating the workspace.
#[derive(Debug, PartialEq, Eq)]
enum OutputExclusion {
    /// Leave out this path relative to the project root.
    Exclude(WorkspacePath),
    /// It contains (one of the paths of) the entrypoint, so it holds the
    /// sources too and cannot be left out.
    ContainsEntrypoint,
    /// It is outside the project: nothing to do.
    NotInProject,
}

/// Decides about the output directory `dir` (canonical) for the project
/// `root` (canonical) with the entrypoint at `entrypoints` (see
/// [`entrypoint_paths`]).
///
/// Both paths are canonical, so `dir` is spelled as stored on disk; the
/// workspace layer also matches other spellings of it (case, Unicode
/// normalization). A directory that does not exist yet has nothing to copy
/// and is not passed here.
fn output_exclusion(dir: &Path, root: &Path, entrypoints: &[WorkspacePath]) -> OutputExclusion {
    let Ok(rel) = dir.strip_prefix(root) else {
        return OutputExclusion::NotInProject;
    };
    if rel.as_os_str().is_empty() {
        return OutputExclusion::ContainsEntrypoint;
    }
    let Ok(rel) = WorkspacePath::from_path(rel) else {
        return OutputExclusion::NotInProject;
    };
    if entrypoints.iter().any(|e| e.starts_with(&rel)) {
        OutputExclusion::ContainsEntrypoint
    } else {
        OutputExclusion::Exclude(rel)
    }
}

/// What [`copy_artifacts`] did.
struct CopyOutcome {
    /// Artifacts now in `dir`, with sizes.
    copied: Vec<Artifact>,
    /// Artifacts that were not copied because of `error`.
    not_copied: Vec<Artifact>,
    /// The canonical output directory, once it was prepared.
    dir: Option<PathBuf>,
    error: Option<ErrorInfo>,
}

/// Copies the artifacts to `output` one by one, stopping at the first
/// failure, so the report can say exactly which ones reached the host.
fn copy_artifacts(
    ws: &Workspace,
    artifacts: &[Artifact],
    output: &Path,
    project_root: &Path,
) -> CopyOutcome {
    let mut copied = CopyOutcome {
        copied: Vec::new(),
        not_copied: Vec::new(),
        dir: None,
        error: None,
    };
    if artifacts.is_empty() {
        return copied;
    }
    let dest = match output::walk(output, project_root, true) {
        Ok(Some(dest)) => dest,
        Ok(None) => unreachable!("walk with create returns the directory"),
        Err(e) => {
            copied.not_copied = artifacts.to_vec();
            copied.error = Some(output_error(output, e));
            return copied;
        }
    };
    let mut seen = HashSet::new();
    for (i, artifact) in artifacts.iter().enumerate() {
        if !seen.insert(&artifact.path) {
            continue;
        }
        match ws.collect_artifacts(
            std::slice::from_ref(artifact),
            &dest,
            OverwritePolicy::Replace,
        ) {
            Ok(done) => copied.copied.extend(done),
            Err(e) => {
                copied.not_copied = artifacts[i..].to_vec();
                copied.error = Some(ErrorInfo::from_workspace(Stage::Collect, &e));
                break;
            }
        }
    }
    copied.dir = Some(dest);
    copied
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
        let note = check_root(&temp, true).unwrap().unwrap();
        assert_eq!(note.kind, "broad_project_root");
        let project = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(project.path()).unwrap();
        assert!(check_root(&project, false).unwrap().is_none());
    }

    #[test]
    fn output_inside_the_root_is_excluded_by_path() {
        use OutputExclusion::{ContainsEntrypoint, Exclude, NotInProject};
        let root = Path::new("/p");
        let wp = |s: &str| WorkspacePath::new(s).unwrap();
        // `main.tex` is a symlink to `a/b/real.tex`.
        let entries = [wp("src/main.tex"), wp("a/b/real.tex")];
        let decide = |dir: &str| output_exclusion(Path::new(dir), root, &entries);
        assert_eq!(decide("/p/build"), Exclude(wp("build")));
        assert_eq!(decide("/p/build/pdf"), Exclude(wp("build/pdf")));
        assert_eq!(
            decide("/p/src/texrun-out/a/b"),
            Exclude(wp("src/texrun-out/a/b"))
        );
        assert_eq!(decide("/p/srcs"), Exclude(wp("srcs")));
        assert_eq!(decide("/p/a/bb"), Exclude(wp("a/bb")));
        assert_eq!(decide("/q/build"), NotInProject);
        assert_eq!(decide("/pp/build"), NotInProject);
        // The root itself, or containing the entrypoint as given or resolved.
        assert_eq!(decide("/p"), ContainsEntrypoint);
        assert_eq!(decide("/p/src"), ContainsEntrypoint);
        assert_eq!(decide("/p/a/b"), ContainsEntrypoint);
        assert_eq!(decide("/p/a"), ContainsEntrypoint);
    }
}
