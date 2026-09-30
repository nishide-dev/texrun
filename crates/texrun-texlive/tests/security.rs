//! Security fixtures: the guarantees of docs/security.md §2 hold for a
//! compile through the workspace and the TeX Live engine.
//!
//! Each test only checks that a defence is in effect; `tests/fixtures/
//! security/` holds the minimal inputs. Where a mechanism can be allowed
//! (reads and writes inside the workspace), a control compile checks that
//! a refusal is not the result of a broken fixture.
//!
//! See `tests/common/mod.rs` for how these tests are enabled.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{
    Compile, assert_outcome, assert_pdf, copy_tree, describe, files_named, find, fixture, has,
    host_only, path_with, plain_tempdir, require_texlive, which,
};
use tempfile::TempDir;
use texrun_core::{CompileOptions, CompileOutcome, DiagnosticKind};
use texrun_texlive::{LatexmkConfig, LatexmkEngine};
use texrun_workspace::{ExclusionReason, ProjectInput, Workspace, WorkspaceConfig, WorkspaceError};

const MARKER: &str = "shell-escape-marker";

/// Asserts that no file named like `prefix` exists below `dir`.
fn assert_no_file(dir: &Path, prefix: &str) {
    let found = files_named(dir, prefix);
    assert!(found.is_empty(), "unexpected files: {found:?}");
}

// --- 1. no command execution -------------------------------------------------

#[test]
fn shell_escape_write18_is_not_executed() {
    require_texlive!();
    let (run, ws) = Compile::fixture("security/shell-escape", "write18.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_no_file(ws.path(), MARKER);
    // Only the log says whether TeX tried; this is a single stable line.
    let log = fs::read_to_string(ws.output_dir().join("write18.log")).unwrap();
    assert!(log.contains("runsystem(touch"), "{log}");
    assert!(log.contains("disabled"), "{log}");
}

#[test]
fn shell_escape_pipe_input_is_not_executed() {
    require_texlive!();
    let (run, ws) = Compile::fixture("security/shell-escape", "pipe-input.tex").run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert_no_file(ws.path(), MARKER);
}

#[test]
fn rc_files_in_the_project_are_not_copied_or_read() {
    require_texlive!();
    let (run, ws) = Compile::fixture("security/rc-files", "main.tex").run();
    // Workspace layer: both rc files are left out and reported.
    let mut excluded: Vec<_> = ws
        .report()
        .excluded_with(ExclusionReason::ToolConfig)
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    excluded.sort();
    assert_eq!(excluded, [".latexmkrc", "latexmkrc"]);
    assert!(!ws.path().join("latexmkrc").exists());
    assert_outcome(&run, CompileOutcome::Succeeded);

    // Engine layer (`-norc`): rc files placed after the copy in the
    // workspace and in the user rc locations of the engine's HOME
    // (`~/.latexmkrc`, `~/.config/latexmk/latexmkrc`) are not read either.
    // Reading any of them would fail the compile.
    let ws = Compile::fixture("security/rc-files", "main.tex").workspace();
    let rc = fs::read(fixture("security/rc-files/latexmkrc")).unwrap();
    let home = ws.path().join(".texrun/home");
    let xdg = home.join(".config/latexmk");
    fs::create_dir_all(&xdg).unwrap();
    for dir in [ws.path(), home.as_path()] {
        fs::write(dir.join("latexmkrc"), &rc).unwrap();
        fs::write(dir.join(".latexmkrc"), &rc).unwrap();
    }
    fs::write(xdg.join("latexmkrc"), &rc).unwrap();
    let run = Compile::fixture("security/rc-files", "main.tex").run_in(&ws);
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_pdf(&run, &ws, "main.pdf");
}

#[test]
fn format_line_is_ignored() {
    // The control runs the host's pdflatex.
    host_only!();
    require_texlive!();
    // The first line names an installed format without LaTeX. Honoured, it
    // would replace the LaTeX format and the document would fail.
    let (run, ws) = Compile::fixture("security/format-line", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_pdf(&run, &ws, "main.pdf");

    // Control: pdflatex run directly with the first line honoured fails on
    // the same file, so the fixture does exercise the first line.
    let dir = plain_tempdir();
    copy_tree(&fixture("security/format-line"), dir.path());
    let status = Command::new(which("pdflatex"))
        .args([
            "-parse-first-line",
            "-no-shell-escape",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "main.tex",
        ])
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "the first line had no effect: {status:?}"
    );
}

/// Writes wrapper scripts for pdflatex / bibtex / makeindex into `dir`. Each
/// records the command name of its parent process and its arguments (one per
/// line) in `<dir>/calls-<tool>`, then runs the real tool.
fn write_recording_wrappers(dir: &Path) {
    for tool in ["pdflatex", "bibtex", "makeindex"] {
        let real = which(tool);
        let log = dir.join(format!("calls-{tool}"));
        let script = format!(
            "#!/bin/sh\n\
             {{ echo \"parent=$(ps -o comm= -p \"$PPID\")\"; for a in \"$@\"; do echo \"arg=$a\"; done; echo end; }} >> '{log}'\n\
             exec '{real}' \"$@\"\n",
            log = log.display(),
            real = real.display(),
        );
        let path = dir.join(tool);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Recorded calls of `tool`: (parent command name, arguments).
fn recorded_calls(dir: &Path, tool: &str) -> Vec<(String, Vec<String>)> {
    let text = fs::read_to_string(dir.join(format!("calls-{tool}"))).unwrap_or_default();
    let mut calls = Vec::new();
    let mut parent = String::new();
    let mut args = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("parent=") {
            // `comm` may be a full path on some systems.
            p.trim()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .clone_into(&mut parent);
        } else if let Some(a) = line.strip_prefix("arg=") {
            args.push(a.to_owned());
        } else if line == "end" {
            calls.push((std::mem::take(&mut parent), std::mem::take(&mut args)));
        }
    }
    calls
}

#[test]
fn auxiliary_tools_are_started_without_a_shell() {
    // Wrappers in the host PATH; the container backend runs the same rc
    // (tests/container.rs compiles this fixture there).
    host_only!();
    require_texlive!();
    let tools = plain_tempdir();
    write_recording_wrappers(tools.path());
    let engine =
        LatexmkEngine::new(LatexmkConfig::default().with_search_path(path_with(tools.path())));
    let (run, ws) = Compile::fixture("security/aux-tools", "文献 main.tex")
        .engine(engine)
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert!(
        !has(&run, DiagnosticKind::UndefinedCitation),
        "{}",
        describe(&run)
    );
    let out = ws.output_dir();
    assert!(
        fs::read_to_string(out.join("文献 main.bbl"))
            .unwrap()
            .contains("knuth")
    );
    assert!(
        fs::read_to_string(out.join("文献 main.ind"))
            .unwrap()
            .contains("word")
    );

    for tool in ["pdflatex", "bibtex", "makeindex"] {
        let calls = recorded_calls(tools.path(), tool);
        assert!(!calls.is_empty(), "{tool} was not run through the wrapper");
        for (parent, args) in &calls {
            // Started by latexmk itself, not by a shell it spawned.
            assert_eq!(parent, "latexmk", "{tool} {args:?}");
            // The file name with a space reached the tool as one argument.
            assert!(
                args.iter().any(|a| a.contains("文献 main")),
                "{tool} {args:?}"
            );
        }
    }
    let pdflatex = recorded_calls(tools.path(), "pdflatex");
    for (_, args) in &pdflatex {
        assert!(args.iter().any(|a| a == "-no-shell-escape"), "{args:?}");
        assert!(args.iter().any(|a| a == "-no-parse-first-line"), "{args:?}");
    }
}

// --- 2. no reads / writes outside the workspace ------------------------------

/// A directory `P` holding `P/project` (a copy of `security/outside-paths`
/// whose `probe-path.tex` defines `\probepath` as `probe_path`) and
/// `P/outside-probe.tex`. Workspaces are created directly in `P`, so `..`
/// from the workspace root is `P`.
struct OutsidePaths {
    dir: TempDir,
}

impl OutsidePaths {
    fn new(probe_path: impl FnOnce(&Path) -> String) -> Self {
        let dir = plain_tempdir();
        let project = dir.path().join("project");
        copy_tree(&fixture("security/outside-paths"), &project);
        fs::copy(
            project.join("probe.tex"),
            dir.path().join("outside-probe.tex"),
        )
        .unwrap();
        let path = probe_path(dir.path());
        fs::write(
            project.join("probe-path.tex"),
            format!("\\def\\probepath{{{path}}}\n"),
        )
        .unwrap();
        Self { dir }
    }

    fn compile<'a>(&self, entry: &'a str) -> Compile<'a> {
        Compile::new(self.dir.path().join("project"), entry)
            .config(WorkspaceConfig::default().with_temp_parent(self.dir.path()))
    }
}

fn absolute(dir: &Path, name: &str) -> String {
    let dir = fs::canonicalize(dir).unwrap();
    format!("{}/{name}", dir.display())
}

/// `\input` of `probe_path` is refused: the probe's undefined command is
/// never seen.
fn assert_read_refused(entry: &str, probe_path: impl FnOnce(&Path) -> String) {
    let paths = OutsidePaths::new(probe_path);
    let (run, _ws) = paths.compile(entry).run();
    assert!(
        !has(&run, DiagnosticKind::UndefinedControlSequence),
        "the outside file was read: {}",
        describe(&run)
    );
    if entry == "read.tex" {
        assert_outcome(&run, CompileOutcome::Failed);
        find(&run, DiagnosticKind::MissingFile);
    } else {
        assert_outcome(&run, CompileOutcome::Succeeded);
    }
}

#[test]
fn reading_inside_the_workspace_works() {
    require_texlive!();
    // Control: the probe's undefined command is reported when it is read.
    for entry in ["read.tex", "openin.tex"] {
        let paths = OutsidePaths::new(|_| "probe".to_owned());
        let (run, _ws) = paths.compile(entry).run();
        assert_outcome(&run, CompileOutcome::Failed);
        let d = find(&run, DiagnosticKind::UndefinedControlSequence);
        common::assert_location(d, "probe.tex", 1);
    }
}

#[test]
fn input_of_an_absolute_path_is_refused() {
    require_texlive!();
    assert_read_refused("read.tex", |dir| absolute(dir, "outside-probe"));
}

#[test]
fn input_through_parent_directory_is_refused() {
    require_texlive!();
    assert_read_refused("read.tex", |_| "../outside-probe".to_owned());
}

#[test]
fn openin_of_an_absolute_path_is_refused() {
    require_texlive!();
    assert_read_refused("openin.tex", |dir| absolute(dir, "outside-probe"));
}

#[test]
fn openin_through_parent_directory_is_refused() {
    require_texlive!();
    assert_read_refused("openin.tex", |_| "../outside-probe".to_owned());
}

const WRITTEN: &str = "written-by-tex";

#[test]
fn writing_inside_the_output_directory_works() {
    require_texlive!();
    // Control: a relative `\openout` lands in the output directory.
    let paths = OutsidePaths::new(|_| format!("{WRITTEN}.txt"));
    let (run, ws) = paths.compile("write.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    // Canonical on both sides: the temporary directory may be reached
    // through a symlink (`/var` -> `/private/var` on macOS).
    let found: Vec<_> = files_named(paths.dir.path(), WRITTEN)
        .iter()
        .map(|p| fs::canonicalize(p).unwrap())
        .collect();
    let expected = fs::canonicalize(ws.output_dir().join(format!("{WRITTEN}.txt"))).unwrap();
    assert_eq!(found, [expected]);
}

fn assert_write_refused(probe_path: impl FnOnce(&Path) -> String) {
    let paths = OutsidePaths::new(probe_path);
    let (run, _ws) = paths.compile("write.tex").run();
    // Nothing named like the target anywhere: not outside, and not
    // redirected into the workspace either.
    assert_no_file(paths.dir.path(), WRITTEN);
    assert_ne!(
        run.result.outcome,
        CompileOutcome::TimedOut,
        "{}",
        describe(&run)
    );
}

#[test]
fn openout_to_an_absolute_path_is_refused() {
    require_texlive!();
    assert_write_refused(|dir| absolute(dir, &format!("{WRITTEN}.txt")));
}

#[test]
fn openout_through_parent_directory_is_refused() {
    require_texlive!();
    assert_write_refused(|_| format!("../../../{WRITTEN}.txt"));
}

#[test]
fn symlink_outside_the_root_is_rejected_before_compiling() {
    // Workspace layer only; no TeX needed.
    let dir = plain_tempdir();
    let project = dir.path().join("project");
    copy_tree(&fixture("minimal"), &project);
    fs::write(dir.path().join("outside.tex"), "outside\n").unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside.tex"), project.join("link.tex")).unwrap();
    let input = ProjectInput::new(&project, "main.tex").unwrap();
    let err = Workspace::create(
        &input,
        CompileOptions::default(),
        &WorkspaceConfig::default(),
    )
    .unwrap_err();
    assert!(
        matches!(err, WorkspaceError::SymlinkOutsideRoot { .. }),
        "{err:?}"
    );
}
