//! Integration tests against a real TeX Live + latexmk.
//!
//! These are `#[ignore]`d until #10 decides how TeX Live tests are enabled.
//! Run them in the dev container (docs/development.md):
//!
//! ```text
//! docker-compose run --rm dev cargo test -p texrun-texlive -- --ignored
//! ```
//!
//! They fail (rather than skip) when latexmk is missing.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use texrun_core::{
    ArtifactKind, CancelToken, CompileOptions, CompileOutcome, DiagnosticKind, EngineError,
    TypesetEngine,
};
use texrun_texlive::{LatexmkConfig, LatexmkEngine, LatexmkRun, Limits};
use texrun_workspace::{ProjectInput, Workspace, WorkspaceConfig};

const NEEDS_TEX: &str =
    "requires TeX Live with latexmk; run with --ignored (see docs/development.md)";

const MINIMAL: &str =
    "\\documentclass{article}\n\\begin{document}\nHello, texrun.\n\\end{document}\n";

/// A doc that never finishes: `\x` expands to itself forever.
const INFINITE_LOOP: &str =
    "\\documentclass{article}\n\\begin{document}\n\\def\\x{\\x}\\x\n\\end{document}\n";

fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn workspace(project: &TempDir, entry: &str, options: CompileOptions) -> Workspace {
    let input = ProjectInput::new(project.path(), entry).unwrap();
    Workspace::create(&input, options, &WorkspaceConfig::default()).unwrap()
}

fn run_with(
    engine: &LatexmkEngine,
    files: &[(&str, &str)],
    entry: &str,
    options: CompileOptions,
    cancel: Option<CancelToken>,
) -> (LatexmkRun, Workspace, TempDir) {
    let project = project(files);
    let ws = workspace(&project, entry, options);
    let mut ctx = ws.context();
    if let Some(cancel) = cancel {
        ctx = ctx.with_cancel(cancel);
    }
    let run = engine.run(&ctx, ws.request()).unwrap();
    (run, ws, project)
}

fn run(files: &[(&str, &str)], entry: &str) -> (LatexmkRun, Workspace, TempDir) {
    run_with(
        &LatexmkEngine::default(),
        files,
        entry,
        CompileOptions::default().with_timeout(Duration::from_secs(60)),
        None,
    )
}

fn describe(run: &LatexmkRun) -> String {
    format!(
        "outcome {:?}, exit {:?}, diagnostics {:#?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        run.result.outcome,
        run.result.exit,
        run.result.diagnostics,
        String::from_utf8_lossy(&run.stdout.bytes),
        String::from_utf8_lossy(&run.stderr.bytes),
    )
}

fn assert_succeeded(run: &LatexmkRun) {
    assert_eq!(
        run.result.outcome,
        CompileOutcome::Succeeded,
        "{}",
        describe(run)
    );
}

/// One process from `ps`.
#[derive(Debug, Clone)]
struct Proc {
    pid: u32,
    ppid: u32,
    pgid: u32,
    stat: String,
    comm: String,
    args: String,
}

fn processes() -> Vec<Proc> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,comm=,args="])
        .output()
        .expect("ps must be available");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            Some(Proc {
                pid: f.next()?.parse().ok()?,
                ppid: f.next()?.parse().ok()?,
                pgid: f.next()?.parse().ok()?,
                stat: f.next()?.to_owned(),
                comm: f.next()?.to_owned(),
                args: f.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// Live (non-zombie) members of process group `pgid`, waiting up to 3 s for
/// `SIGKILL` to take effect. Zombies are ignored: in the dev container
/// PID 1 is not an init process and never reaps reparented children.
fn live_group_members(pgid: u32) -> Vec<Proc> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let members: Vec<Proc> = processes()
            .into_iter()
            .filter(|p| p.pgid == pgid && !p.stat.starts_with('Z'))
            .collect();
        if members.is_empty() || Instant::now() >= deadline {
            return members;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn probe_reports_latexmk_version() {
    let engine = LatexmkEngine::default();
    let info = engine.probe().expect(NEEDS_TEX);
    assert_eq!(info.name, "texlive");
    let version = info.version.unwrap();
    assert!(version.starts_with("latexmk 4."), "{version}");
    assert_eq!(engine.info().version.as_deref(), Some(version.as_str()));
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn minimal_article_produces_a_pdf() {
    let (run, ws, _p) = run(&[("main.tex", MINIMAL)], "main.tex");
    assert_succeeded(&run);
    let result = &run.result;
    assert_eq!(result.exit.unwrap().code, Some(0));
    assert!(result.elapsed > Duration::ZERO);

    let pdf = result.pdf().expect("PDF artifact");
    assert_eq!(pdf.path.as_str(), "main.pdf");
    let bytes = fs::read(ws.output_dir().join("main.pdf")).unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    assert_eq!(pdf.size_bytes, Some(bytes.len() as u64));
    assert_eq!(result.log().unwrap().path.as_str(), "main.log");

    // Only PDF and log are reported, never .aux / .fls / .fdb_latexmk.
    assert_eq!(result.artifacts.len(), 2, "{:?}", result.artifacts);
    assert!(ws.output_dir().join("main.fls").exists());
    assert_eq!(result.errors().count(), 0, "{}", describe(&run));

    // The rc directory is gone and nothing was written outside the output
    // directory and HOME.
    let texrun_dir = ws.path().join(".texrun");
    let mut names: Vec<_> = fs::read_dir(&texrun_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["home", "out"]);
    let mut top: Vec<_> = fs::read_dir(ws.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    top.sort();
    assert_eq!(top, [".texrun", "main.tex"]);
    assert!(
        !String::from_utf8_lossy(&run.stdout.bytes).contains("texrun: start signal missing"),
        "{}",
        describe(&run)
    );
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn document_error_is_a_failed_outcome_with_diagnostics() {
    let doc =
        "\\documentclass{article}\n\\begin{document}\nText.\n\\undefinedmacro\n\\end{document}\n";
    let (run, _ws, _p) = run(&[("main.tex", doc)], "main.tex");
    let result = &run.result;
    assert_eq!(result.outcome, CompileOutcome::Failed, "{}", describe(&run));
    assert_ne!(result.exit.unwrap().code, Some(0));
    assert!(result.log().is_some());
    let d = result
        .errors()
        .find(|d| d.kind == DiagnosticKind::UndefinedControlSequence)
        .unwrap_or_else(|| panic!("{}", describe(&run)));
    assert_eq!(
        d.file.as_ref().map(texrun_core::WorkspacePath::as_str),
        Some("main.tex")
    );
    assert_eq!(d.line, Some(4));
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn missing_latexmk_is_unavailable() {
    // Everything else is in place; only latexmk cannot be found.
    let empty = tempfile::tempdir().unwrap();
    let engine = LatexmkEngine::new(LatexmkConfig::default().with_search_path(empty.path()));
    let project = project(&[("main.tex", MINIMAL)]);
    let ws = workspace(&project, "main.tex", CompileOptions::default());
    let err = engine.compile(&ws.context(), ws.request()).unwrap_err();
    assert!(matches!(err, EngineError::Unavailable { .. }), "{err:?}");
    assert!(matches!(
        engine.probe(),
        Err(EngineError::Unavailable { .. })
    ));
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn timeout_stops_the_whole_process_tree() {
    let started = Instant::now();
    let (run, _ws, _p) = run_with(
        &LatexmkEngine::default(),
        &[("main.tex", INFINITE_LOOP)],
        "main.tex",
        CompileOptions::default().with_timeout(Duration::from_secs(3)),
        None,
    );
    assert_eq!(
        run.result.outcome,
        CompileOutcome::TimedOut,
        "{}",
        describe(&run)
    );
    assert_eq!(run.result.exit.unwrap().signal, Some(9));
    assert!(started.elapsed() < Duration::from_secs(20));
    let left = live_group_members(run.pid);
    assert!(left.is_empty(), "processes left behind: {left:#?}");
    let pdflatex_left: Vec<_> = processes()
        .into_iter()
        .filter(|p| p.comm == "pdflatex" && p.ppid == run.pid && !p.stat.starts_with('Z'))
        .collect();
    assert!(pdflatex_left.is_empty(), "{pdflatex_left:#?}");
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn cancel_stops_the_whole_process_tree() {
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        trigger.cancel();
    });
    let started = Instant::now();
    let (run, _ws, _p) = run_with(
        &LatexmkEngine::default(),
        &[("main.tex", INFINITE_LOOP)],
        "main.tex",
        CompileOptions::default().with_timeout(Duration::from_secs(60)),
        Some(cancel),
    );
    t.join().unwrap();
    assert_eq!(
        run.result.outcome,
        CompileOutcome::Cancelled,
        "{}",
        describe(&run)
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    let left = live_group_members(run.pid);
    assert!(left.is_empty(), "processes left behind: {left:#?}");
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn no_shell_between_latexmk_and_pdflatex() {
    // A unique name identifies this test's pdflatex among parallel tests.
    let entry = "shelltree-probe.tex";
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
    let (run, _ws, _p) = run_with(
        &LatexmkEngine::default(),
        &[(entry, INFINITE_LOOP)],
        entry,
        CompileOptions::default().with_timeout(Duration::from_secs(60)),
        Some(cancel),
    );
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
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn subdirectory_entrypoint_with_input_and_include() {
    let main = "\\documentclass{article}\n\\input{macros}\n\\begin{document}\n\\hello\n\\include{chapters/intro}\n\\end{document}\n";
    let intro =
        "\\section{Intro}\\label{sec:intro}\nSee \\ref{sec:intro} and \\ref{sec:missing}.\n";
    let (run, ws, _p) = run(
        &[
            ("src/main.tex", main),
            ("src/macros.tex", "\\newcommand{\\hello}{Hello.}\n"),
            ("src/chapters/intro.tex", intro),
            ("README.md", "not part of the document\n"),
        ],
        "src/main.tex",
    );
    assert_succeeded(&run);
    let out = ws.output_dir();
    assert!(out.join("main.pdf").is_file());
    assert!(out.join("chapters/intro.aux").is_file());
    // Nothing was written next to the sources.
    assert!(!ws.path().join("src/main.aux").exists());
    let d = run
        .result
        .warnings()
        .find(|d| d.kind == DiagnosticKind::UndefinedReference)
        .unwrap_or_else(|| panic!("{}", describe(&run)));
    assert!(d.message.contains("sec:missing"), "{d:?}");
    assert_eq!(
        d.file.as_ref().map(texrun_core::WorkspacePath::as_str),
        Some("src/chapters/intro.tex"),
        "{d:?}"
    );
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn japanese_and_space_file_names() {
    let (run, ws, _p) = run(&[("論文 ドラフト.tex", MINIMAL)], "論文 ドラフト.tex");
    assert_succeeded(&run);
    assert_eq!(run.result.pdf().unwrap().path.as_str(), "論文 ドラフト.pdf");
    assert!(ws.output_dir().join("論文 ドラフト.pdf").is_file());
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn leading_dash_file_name_is_not_an_option() {
    let (run, _ws, _p) = run(&[("-draft.tex", MINIMAL)], "-draft.tex");
    assert_succeeded(&run);
    assert_eq!(run.result.pdf().unwrap().path.as_str(), "-draft.pdf");
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn bibtex_citations_are_resolved() {
    let main = "\\documentclass{article}\n\\begin{document}\nSee \\cite{knuth}.\n\\bibliographystyle{plain}\n\\bibliography{refs}\n\\end{document}\n";
    let bib = "@book{knuth,\n  author = {Donald E. Knuth},\n  title = {The {\\TeX}book},\n  publisher = {Addison-Wesley},\n  year = {1984}\n}\n";
    let (run, ws, _p) = run(&[("main.tex", main), ("refs.bib", bib)], "main.tex");
    assert_succeeded(&run);
    let bbl = fs::read_to_string(ws.output_dir().join("main.bbl")).unwrap();
    assert!(bbl.contains("knuth"), "{bbl}");
    assert!(
        !run.result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::UndefinedCitation),
        "{}",
        describe(&run)
    );
    // kpsewhich is disabled; latexmk says so on stderr and carries on. The
    // message is not turned into a diagnostic (only the .log is parsed).
    let stderr = String::from_utf8_lossy(&run.stderr.bytes);
    assert!(
        stderr.contains("Kpsewhich command needed but not set up"),
        "{}",
        describe(&run)
    );
    assert!(
        !stderr.contains("texrun: failed to run"),
        "{}",
        describe(&run)
    );
    assert!(
        !run.result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("Kpsewhich")),
        "{}",
        describe(&run)
    );
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn makeindex_runs_without_a_shell() {
    let main = "\\documentclass{article}\n\\usepackage{makeidx}\n\\makeindex\n\\begin{document}\nWord\\index{word}.\n\\printindex\n\\end{document}\n";
    let (run, ws, _p) = run(&[("main.tex", main)], "main.tex");
    assert_succeeded(&run);
    let ind = fs::read_to_string(ws.output_dir().join("main.ind")).unwrap();
    assert!(ind.contains("word"), "{ind}");
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn shell_escape_is_disabled() {
    let doc = "\\documentclass{article}\n\\begin{document}\n\\immediate\\write18{touch marker}\nText.\n\\end{document}\n";
    let (run, ws, _p) = run(&[("main.tex", doc)], "main.tex");
    assert_succeeded(&run);
    assert!(!ws.path().join("marker").exists());
    assert!(!ws.output_dir().join("marker").exists());
    let log = fs::read_to_string(ws.output_dir().join("main.log")).unwrap();
    assert!(log.contains("runsystem(touch marker)...disabled"), "{log}");
}

/// Writes the log without end.
const ENDLESS_LOG: &str = "\\documentclass{article}\n\\begin{document}\n\\def\\msg{\\message{texrun output limit test line}\\msg}\\msg\n\\end{document}\n";

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn output_directory_limit_stops_the_compile() {
    let limits = Limits::default()
        .with_max_output_bytes(2 * 1024 * 1024)
        .with_size_check_interval(Duration::from_millis(200));
    let engine = LatexmkEngine::new(LatexmkConfig::default().with_limits(limits));
    let (run, _ws, _p) = run_with(
        &engine,
        &[("main.tex", ENDLESS_LOG)],
        "main.tex",
        CompileOptions::default().with_timeout(Duration::from_secs(60)),
        None,
    );
    assert_eq!(
        run.result.outcome,
        CompileOutcome::Failed,
        "{}",
        describe(&run)
    );
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
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn per_file_limit_stops_the_compile() {
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
    let (run, ws, _p) = run_with(
        &engine,
        &[("main.tex", ENDLESS_LOG)],
        "main.tex",
        CompileOptions::default().with_timeout(Duration::from_secs(60)),
        None,
    );
    assert_eq!(
        run.result.outcome,
        CompileOutcome::Failed,
        "{}",
        describe(&run)
    );
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
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn workspace_rc_and_home_rc_are_not_read() {
    // `-norc`: an rc in the project would be excluded by the workspace layer
    // anyway; here one is placed in texrun's HOME to check latexmk ignores
    // user rc files too. It would turn the compile into a failure.
    let project = project(&[("main.tex", MINIMAL)]);
    let ws = workspace(&project, "main.tex", CompileOptions::default());
    let home = ws.path().join(".texrun/home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".latexmkrc"), "die \"user rc was read\\n\";\n").unwrap();
    let run = LatexmkEngine::default()
        .run(&ws.context(), ws.request())
        .unwrap();
    assert_succeeded(&run);
    assert_eq!(
        run.result.artifacts_of(ArtifactKind::Pdf).count(),
        1,
        "{}",
        describe(&run)
    );
}
