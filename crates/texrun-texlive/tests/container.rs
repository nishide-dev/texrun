//! What the container backend guarantees on top of the host engine
//! (docs/security.md §2, §4): the rest of the host is not visible to TeX,
//! containers never outlive the compile, and the limits are enforced by
//! the runtime.
//!
//! The #10 fixtures themselves run with the container backend through
//! `TEXRUN_TEST_BACKEND=container` (`scenarios.rs`, `security.rs`). These
//! tests always use it; they are skipped without a container runtime and
//! the engine image, unless `TEXRUN_REQUIRE_SANDBOX=1`. See
//! `tests/common/mod.rs`.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use common::{
    Compile, TestEngine, assert_location, assert_outcome, assert_pdf, container_engine, copy_tree,
    describe, find, fixture, has, host_only, plain_tempdir, require_sandbox, require_texlive,
};
use tempfile::TempDir;
use texrun_core::{
    CancelToken, CompileOutcome, DiagnosticKind, EngineErrorKind, PathMapping, ResourceLimits,
    TypesetEngine,
};
use texrun_texlive::{
    CONTAINER_ENGINE_NAME, ContainerConfig, ContainerEngine, LatexmkEngine, Limits,
};
use texrun_workspace::WorkspaceConfig;

/// The tests check for containers left behind by this process, so they
/// run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn engine() -> ContainerEngine {
    container_engine(ContainerConfig::default())
}

/// Containers created by this test process that still exist.
fn containers_of_this_process() -> Vec<String> {
    let runtime = engine().runtime().unwrap();
    let out = Command::new(runtime.program())
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg(format!(
            "label=org.texrun.sandbox.pid={}",
            std::process::id()
        ))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_result_names_the_container_backend() {
    require_sandbox!();
    let _serial = serial();
    let (run, ws) = Compile::fixture("minimal", "main.tex")
        .engine(engine())
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert_pdf(&run, &ws, "main.pdf");
    assert_eq!(run.result.engine.name, CONTAINER_ENGINE_NAME);
    assert_eq!(
        run.result.resource_limits,
        Some(ResourceLimits::new(true, true))
    );
    assert!(containers_of_this_process().is_empty());

    let engine = engine();
    let info = engine.probe().unwrap();
    let version = info.version.unwrap();
    assert!(version.starts_with("latexmk "), "{version}");
    assert!(version.contains("image "), "{version}");
}

/// `P/project` is the `security/sandbox` fixture with `probe-path.tex`
/// naming `probe_path`; `P/outside-probe.txt` is a host file outside the
/// workspace.
struct Embedded {
    _dir: TempDir,
    outcome: CompileOutcome,
    /// The PDF (empty if there is none).
    pdf: Vec<u8>,
    /// The main log (empty if there is none).
    log: String,
    /// The whole run, for failure messages.
    run: String,
}

/// Compiles the `security/sandbox` fixture with `engine`, the probe path
/// being `probe_path(P)`.
fn embed(engine: TestEngine, probe_path: impl FnOnce(&Path) -> String) -> Embedded {
    let dir = plain_tempdir();
    let project = dir.path().join("project");
    copy_tree(&fixture("security/sandbox"), &project);
    fs::write(
        dir.path().join("outside-probe.txt"),
        "texrun-outside-marker\n",
    )
    .unwrap();
    let path = probe_path(&fs::canonicalize(dir.path()).unwrap());
    fs::write(
        project.join("probe-path.tex"),
        format!("\\def\\probepath{{{path}}}\n"),
    )
    .unwrap();
    let (run, ws) = Compile::new(&project, "embed.tex")
        .config(WorkspaceConfig::default().with_temp_parent(dir.path()))
        .engine(engine)
        .run();
    let embedded = Embedded {
        outcome: run.result.outcome,
        pdf: fs::read(ws.output_dir().join("embed.pdf")).unwrap_or_default(),
        log: fs::read_to_string(ws.output_dir().join("embed.log")).unwrap_or_default(),
        run: describe(&run),
        _dir: dir,
    };
    drop(ws);
    embedded
}

/// The absolute host path of the file outside the workspace, and the same
/// through `..` from the workspace root and from its parent.
const OUTSIDE_PROBES: [fn(&Path) -> String; 3] = [
    |dir: &Path| format!("{}/outside-probe.txt", dir.display()),
    |_| "../outside-probe.txt".to_owned(),
    |_| "../../outside-probe.txt".to_owned(),
];

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn files_outside_the_workspace_cannot_be_embedded() {
    require_sandbox!();
    let _serial = serial();
    // Control: a file inside the workspace is embedded into the PDF.
    let inside = embed(engine().into(), |_| "inside-probe.txt".to_owned());
    assert_eq!(inside.outcome, CompileOutcome::Succeeded, "{}", inside.run);
    assert!(
        contains(&inside.pdf, "texrun-inside-marker"),
        "{}",
        inside.run
    );

    // The same primitive with a host path outside the workspace, absolute
    // or through `..`: the host file does not exist in the container, so
    // pdfTeX stops because it cannot open it (and not for another reason).
    for probe in OUTSIDE_PROBES {
        let outside = embed(engine().into(), probe);
        assert!(
            !contains(&outside.pdf, "texrun-outside-marker"),
            "{}",
            outside.run
        );
        assert_eq!(outside.outcome, CompileOutcome::Failed, "{}", outside.run);
        assert!(
            outside.log.contains("cannot open file for embedding"),
            "{}\n{}",
            outside.run,
            outside.log
        );
    }
}

/// Control for the test above: on the host, this primitive is one of the
/// reads kpathsea's paranoid mode does not check (docs/security.md §2,
/// "保証しない"), so the same fixture does reach the outside file. This
/// shows that the test above can tell a leak from a refusal.
#[test]
fn the_host_backend_does_not_hide_the_host_from_file_embedding() {
    host_only!();
    require_texlive!();
    let _serial = serial();
    let leaked = embed(LatexmkEngine::default().into(), OUTSIDE_PROBES[0]);
    assert!(
        contains(&leaked.pdf, "texrun-outside-marker"),
        "{}",
        leaked.run
    );
}

#[test]
fn the_texmf_tree_is_the_images() {
    require_sandbox!();
    let _serial = serial();
    // The log names the class file TeX read: from the image's TeX Live, at
    // the same place whatever the host has.
    let (run, ws) = Compile::fixture("minimal", "main.tex")
        .engine(engine())
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let log = fs::read_to_string(ws.output_dir().join("main.log")).unwrap();
    assert!(
        log.contains("/usr/share/texlive/texmf-dist/tex/latex/base/article.cls"),
        "{log}"
    );
}

#[test]
fn a_timeout_leaves_no_container() {
    require_sandbox!();
    let _serial = serial();
    let started = Instant::now();
    let (run, _ws) = Compile::fixture("timeout", "main.tex")
        .engine(engine())
        .timeout(Duration::from_secs(2))
        .run();
    assert_outcome(&run, CompileOutcome::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(containers_of_this_process().is_empty());
}

#[test]
fn cancellation_leaves_no_container() {
    require_sandbox!();
    let _serial = serial();
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        trigger.cancel();
    });
    let (run, _ws) = Compile::fixture("timeout", "main.tex")
        .engine(engine())
        .timeout(Duration::from_secs(60))
        .cancel(cancel)
        .run();
    canceller.join().unwrap();
    assert_outcome(&run, CompileOutcome::Cancelled);
    assert!(containers_of_this_process().is_empty());
}

#[test]
fn the_cpu_time_limit_applies_in_the_container() {
    require_sandbox!();
    let _serial = serial();
    let limits = Limits::default().with_max_cpu_time(Some(Duration::from_secs(2)));
    let (run, _ws) = Compile::fixture("timeout", "main.tex")
        .engine(container_engine(
            ContainerConfig::default().with_limits(limits),
        ))
        .timeout(Duration::from_secs(60))
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::ResourceLimit);
    assert!(d.message.contains("CPU time"), "{d:#?}");
}

/// latexmk cannot start pdflatex within the container's process limit
/// (`--pids-limit`): the processes of the container are the init process,
/// the reporting shell, `timeout` and latexmk. perl retries the refused
/// `fork`, so the timeout usually ends the compile; either way the result
/// says that the limit was reached (read from the container's
/// `pids.events`).
#[test]
fn the_process_limit_is_reported_from_the_container() {
    require_sandbox!();
    let _serial = serial();
    let limits = Limits::default().with_max_processes(4);
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(container_engine(
            ContainerConfig::default().with_limits(limits),
        ))
        .timeout(Duration::from_secs(8))
        .run();
    assert!(
        matches!(
            run.result.outcome,
            CompileOutcome::TimedOut | CompileOutcome::Failed
        ),
        "{}",
        describe(&run)
    );
    let d = find(&run, DiagnosticKind::ResourceLimit);
    assert!(
        d.message.contains("more than 4 processes"),
        "{}",
        describe(&run)
    );
    // The report is not part of latexmk's output.
    assert!(
        !String::from_utf8_lossy(&run.stderr.bytes).contains("texrun-sandbox-pids"),
        "{}",
        describe(&run)
    );
    assert!(containers_of_this_process().is_empty());

    // With the default limit, no such diagnostic.
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(engine())
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert!(
        !run.result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::ResourceLimit),
        "{}",
        describe(&run)
    );
    assert!(
        !String::from_utf8_lossy(&run.stderr.bytes).contains("texrun-sandbox-pids"),
        "{}",
        describe(&run)
    );
    // The report line is not counted as output either.
    assert!(!run.stderr.is_truncated(), "{}", describe(&run));
}

#[test]
fn the_address_space_limit_applies_in_the_container() {
    require_sandbox!();
    let _serial = serial();
    // Too small for pdflatex to start (docs/security.md §3.10).
    let limits = Limits::default().with_max_address_space(64 * 1024 * 1024);
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(container_engine(
            ContainerConfig::default().with_limits(limits),
        ))
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    let d = find(&run, DiagnosticKind::ResourceLimit);
    assert!(d.message.contains("out of memory"), "{d:#?}");
}

#[test]
fn bibtex_and_makeindex_run_in_the_container() {
    require_sandbox!();
    let _serial = serial();
    let (run, ws) = Compile::fixture("security/aux-tools", "文献 main.tex")
        .engine(engine())
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
}

#[test]
fn diagnostics_are_workspace_relative_with_any_mount_point() {
    require_sandbox!();
    let _serial = serial();
    let engine = engine();
    for guest in ["/workspace", "/srv/texrun ws/project"] {
        let compile = Compile::fixture("multi-file", "broken.tex");
        let ws = compile.workspace();
        let ctx = ws
            .context()
            .with_path_mapping(PathMapping::new(guest).unwrap());
        let run = engine.run(&ctx, ws.request()).unwrap();
        assert_outcome(&run, CompileOutcome::Failed);
        let d = find(&run, DiagnosticKind::UndefinedControlSequence);
        assert_location(d, "chapters/broken.tex", 2);
    }
}

#[test]
fn a_mount_point_that_would_hide_the_image_is_refused() {
    // Refused before the runtime is looked for.
    let compile = Compile::fixture("minimal", "main.tex");
    let ws = compile.workspace();
    for guest in [
        "/texrun",
        "/texrun/rc/x",
        "/usr",
        "/usr/bin",
        "/bin",
        "/etc",
        "/lib",
        "/tmp",
        "/proc",
        "/home/u",
        "/srv",
    ] {
        let ctx = ws
            .context()
            .with_path_mapping(PathMapping::new(guest).unwrap());
        let err = engine().run(&ctx, ws.request()).unwrap_err();
        assert_eq!(
            err.kind(),
            EngineErrorKind::InvalidRequest,
            "{guest}: {err}"
        );
    }
}

#[test]
fn the_host_engine_refuses_a_path_mapping() {
    // No TeX needed: refused before latexmk is looked for.
    let compile = Compile::fixture("minimal", "main.tex");
    let ws = compile.workspace();
    let ctx = ws
        .context()
        .with_path_mapping(PathMapping::new("/workspace").unwrap());
    let err = LatexmkEngine::default()
        .compile(&ctx, ws.request())
        .unwrap_err();
    assert_eq!(err.kind(), EngineErrorKind::Unsupported, "{err}");
}

#[test]
fn a_missing_image_is_unavailable() {
    require_sandbox!();
    let engine =
        ContainerEngine::new(ContainerConfig::default().with_image("texrun-no-such-image:0"));
    let err = engine.probe().unwrap_err();
    assert_eq!(err.kind(), EngineErrorKind::Unavailable, "{err}");
}
