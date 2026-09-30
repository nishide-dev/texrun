//! Engine behaviour against a real TeX Live + latexmk: probing, process
//! control and output limits. Document scenarios are in `scenarios.rs`,
//! security guarantees in `security.rs`.
//!
//! See `tests/common/mod.rs` for how these tests are enabled.

mod common;

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    Compile, Proc, assert_outcome, describe, fixture, live_group_members, names_in, processes,
    require_texlive,
};
use tempfile::TempDir;
use texrun_core::{
    CancelToken, CompileOptions, CompileOutcome, DiagnosticKind, EngineError, TypesetEngine,
};
use texrun_texlive::{LatexmkConfig, LatexmkEngine, Limits};
use texrun_workspace::{ProjectInput, Workspace, WorkspaceConfig};

/// A project with the given files in a temporary directory.
fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

#[test]
fn probe_reports_latexmk_version() {
    require_texlive!();
    let engine = LatexmkEngine::default();
    let info = engine.probe().unwrap();
    assert_eq!(info.name, "texlive");
    let version = info.version.unwrap();
    assert!(version.starts_with("latexmk 4."), "{version}");
    assert_eq!(engine.info().version.as_deref(), Some(version.as_str()));
}

#[test]
fn minimal_compile_leaves_only_the_expected_files() {
    require_texlive!();
    let (run, ws) = Compile::fixture("minimal", "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert!(run.result.elapsed > Duration::ZERO);
    // .fls exists but is not an artifact (docs/security.md §3.9).
    assert!(ws.output_dir().join("main.fls").exists());
    assert_eq!(run.result.artifacts.len(), 2, "{:?}", run.result.artifacts);

    // The rc directory is gone and nothing was written outside the output
    // directory and HOME.
    assert_eq!(names_in(&ws.path().join(".texrun")), ["home", "out"]);
    assert_eq!(names_in(ws.path()), [".texrun", "main.tex"]);
    assert!(
        !String::from_utf8_lossy(&run.stdout.bytes).contains("texrun: start signal missing"),
        "{}",
        describe(&run)
    );
}

#[test]
fn missing_latexmk_is_unavailable() {
    // Needs no TeX Live: only latexmk's absence matters.
    let empty = tempfile::tempdir().unwrap();
    let engine = LatexmkEngine::new(LatexmkConfig::default().with_search_path(empty.path()));
    let ws = Compile::fixture("minimal", "main.tex").workspace();
    let err = engine.compile(&ws.context(), ws.request()).unwrap_err();
    assert!(matches!(err, EngineError::Unavailable { .. }), "{err:?}");
    assert!(matches!(
        engine.probe(),
        Err(EngineError::Unavailable { .. })
    ));
}

#[test]
fn cancel_stops_the_whole_process_tree() {
    require_texlive!();
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        trigger.cancel();
    });
    let started = Instant::now();
    let (run, _ws) = Compile::fixture("timeout", "main.tex").cancel(cancel).run();
    t.join().unwrap();
    assert_outcome(&run, CompileOutcome::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(20));
    let left = live_group_members(run.pid);
    assert!(left.is_empty(), "processes left behind: {left:#?}");
}

#[test]
fn no_shell_between_latexmk_and_pdflatex() {
    require_texlive!();
    // A unique name identifies this test's pdflatex among parallel tests.
    let entry = "shelltree-probe.tex";
    let loop_doc = fs::read_to_string(fixture("timeout/main.tex")).unwrap();
    let project = project(&[(entry, &loop_doc)]);
    let cancel = CancelToken::new();
    let watcher_cancel = cancel.clone();
    let watcher = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = None;
        while Instant::now() < deadline {
            let procs = processes();
            if let Some(pdflatex) = procs
                .iter()
                .find(|p| p.comm == "pdflatex" && p.args.contains("shelltree-probe"))
            {
                let group: Vec<Proc> = procs
                    .iter()
                    .filter(|p| p.pgid == pdflatex.pgid)
                    .cloned()
                    .collect();
                seen = Some((pdflatex.clone(), group));
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        watcher_cancel.cancel();
        seen
    });
    let (run, _ws) = Compile::new(project.path(), entry).cancel(cancel).run();
    let (pdflatex, group) = watcher.join().unwrap().expect("pdflatex was never seen");
    assert_eq!(run.result.outcome, CompileOutcome::Cancelled);
    assert_eq!(pdflatex.pgid, run.pid, "pdflatex runs in latexmk's group");
    assert_eq!(
        pdflatex.ppid, run.pid,
        "pdflatex is a direct child of latexmk: {group:#?}"
    );
    let leader = group.iter().find(|p| p.pid == run.pid).unwrap();
    assert_eq!(leader.comm, "latexmk", "{group:#?}");
    for p in &group {
        assert!(
            !matches!(p.comm.as_str(), "sh" | "dash" | "bash" | "zsh"),
            "shell in the process tree: {group:#?}"
        );
    }
    // The arguments reached pdflatex as given (no shell quoting involved).
    assert!(
        pdflatex.args.contains("-no-parse-first-line"),
        "{}",
        pdflatex.args
    );
    assert!(
        pdflatex.args.contains("-no-shell-escape"),
        "{}",
        pdflatex.args
    );
}

#[test]
fn subdirectory_entrypoint_with_input_and_include() {
    require_texlive!();
    let main = "\\documentclass{article}\n\\input{macros}\n\\begin{document}\n\\hello\n\\include{chapters/intro}\n\\end{document}\n";
    let intro =
        "\\section{Intro}\\label{sec:intro}\nSee \\ref{sec:intro} and \\ref{sec:missing}.\n";
    let project = project(&[
        ("src/main.tex", main),
        ("src/macros.tex", "\\newcommand{\\hello}{Hello.}\n"),
        ("src/chapters/intro.tex", intro),
        ("README.md", "not part of the document\n"),
    ]);
    let (run, ws) = Compile::new(project.path(), "src/main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let out = ws.output_dir();
    assert!(out.join("main.pdf").is_file());
    assert!(out.join("chapters/intro.aux").is_file());
    // Nothing was written next to the sources.
    assert!(!ws.path().join("src/main.aux").exists());
    let d = common::find(&run, DiagnosticKind::UndefinedReference);
    assert!(d.message.contains("sec:missing"), "{d:?}");
    common::assert_location(d, "src/chapters/intro.tex", 2);
}

#[test]
fn leading_dash_file_name_is_not_an_option() {
    require_texlive!();
    let minimal = fs::read_to_string(fixture("minimal/main.tex")).unwrap();
    let project = project(&[("-draft.tex", &minimal)]);
    let (run, _ws) = Compile::new(project.path(), "-draft.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_eq!(run.result.pdf().unwrap().path.as_str(), "-draft.pdf");
}

/// Writes the log without end.
const ENDLESS_LOG: &str = "\\documentclass{article}\n\\begin{document}\n\\def\\msg{\\message{texrun output limit test line}\\msg}\\msg\n\\end{document}\n";

#[test]
fn output_directory_limit_stops_the_compile() {
    require_texlive!();
    let limits = Limits::default()
        .with_max_output_bytes(2 * 1024 * 1024)
        .with_size_check_interval(Duration::from_millis(200));
    let engine = LatexmkEngine::new(LatexmkConfig::default().with_limits(limits));
    let project = project(&[("main.tex", ENDLESS_LOG)]);
    let (run, _ws) = Compile::new(project.path(), "main.tex")
        .engine(engine)
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert!(
        run.result
            .errors()
            .any(|d| d.message.contains("output limit exceeded")),
        "{}",
        describe(&run)
    );
    assert!(live_group_members(run.pid).is_empty());
    // stdout keeps only its head.
    assert!(run.stdout.bytes.len() <= Limits::DEFAULT_MAX_CAPTURED_BYTES);
}

#[test]
fn per_file_limit_stops_the_compile() {
    require_texlive!();
    allow_core_dumps();
    let max = 3 * 1024 * 1024;
    // A long check interval, so that on Linux RLIMIT_FSIZE is what stops the
    // writer; elsewhere the size check does.
    let limits = Limits::default()
        .with_max_file_bytes(max)
        .with_size_check_interval(Duration::from_millis(if cfg!(target_os = "linux") {
            60_000
        } else {
            200
        }));
    let engine = LatexmkEngine::new(LatexmkConfig::default().with_limits(limits));
    let project = project(&[("main.tex", ENDLESS_LOG)]);
    let (run, ws) = Compile::new(project.path(), "main.tex")
        .engine(engine)
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert!(
        run.result
            .errors()
            .any(|d| d.message.contains("per-file limit")),
        "{}",
        describe(&run)
    );
    let log_len = fs::metadata(ws.output_dir().join("main.log"))
        .unwrap()
        .len();
    if cfg!(target_os = "linux") {
        assert_eq!(log_len, max, "RLIMIT_FSIZE caps the log exactly");
        assert!(run.result.elapsed < Duration::from_secs(30));
    }
    assert!(live_group_members(run.pid).is_empty());
    // No core dump from the SIGXFSZ, even though texrun's own soft
    // RLIMIT_CORE was raised above (texrun sets RLIMIT_CORE=0 for latexmk on
    // Linux; elsewhere the group is stopped with SIGKILL, which never dumps).
    assert_eq!(
        names_in(ws.path()),
        [".texrun", "main.tex"],
        "unexpected files in the workspace"
    );
}

/// Raises this process's soft `RLIMIT_CORE` to its hard limit, so that a
/// child would dump core unless texrun disables it.
fn allow_core_dumps() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Core);
    let _ = setrlimit(
        Resource::Core,
        Rlimit {
            current: limit.maximum,
            maximum: limit.maximum,
        },
    );
}

#[test]
fn compile_with_default_options() {
    require_texlive!();
    // No explicit timeout: the engine's default applies.
    let input = ProjectInput::new(fixture("minimal"), "main.tex").unwrap();
    let ws = Workspace::create(
        &input,
        CompileOptions::default(),
        &WorkspaceConfig::default(),
    )
    .unwrap();
    let result = LatexmkEngine::default()
        .compile(&ws.context(), ws.request())
        .unwrap();
    assert_eq!(result.outcome, CompileOutcome::Succeeded);
}
