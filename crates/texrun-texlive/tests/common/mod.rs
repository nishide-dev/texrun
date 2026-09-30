//! Shared helpers for the TeX Live integration tests.
//!
//! # Running
//!
//! Tests that need TeX Live start with [`require_texlive!`]. When `latexmk`
//! is not on `PATH` they return early and print a `SKIPPED` line on the real
//! stderr (not captured by the test harness), so
//! `cargo test --workspace --all-features` passes on machines without TeX
//! Live. Set `TEXRUN_REQUIRE_TEXLIVE=1` to turn a missing TeX Live into a
//! failure instead; CI and the dev container
//! do (docs/development.md):
//!
//! ```text
//! docker compose run --rm -e TEXRUN_REQUIRE_TEXLIVE=1 dev cargo test -p texrun-texlive
//! ```
//!
//! # Fixtures
//!
//! `tests/fixtures/<name>/` are small TeX projects. They are used as the
//! project root as they are: [`Workspace::create`] copies them, so the tests
//! never write next to the fixture sources. Tests check structured results
//! (outcome, diagnostic kind / file / line, artifacts, PDF page count), never
//! the full log text, so minor TeX Live differences do not break them.

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use texrun_core::{
    CancelToken, CompileOptions, CompileOutcome, Diagnostic, DiagnosticKind, TypesetEngine,
    WorkspacePath,
};
use texrun_preview::{PreviewOptions, Previewer};
use texrun_texlive::{LatexmkEngine, LatexmkRun};
use texrun_workspace::{ProjectInput, Workspace, WorkspaceConfig};

/// Set to `1` to fail (instead of skip) tests that need TeX Live.
pub const REQUIRE_TEXLIVE_ENV: &str = "TEXRUN_REQUIRE_TEXLIVE";

/// Set to `1` to fail (instead of skip) checks that need a preview tool
/// (page counts, previews). The same variable as in the `texrun-preview`
/// real tool tests.
pub const REQUIRE_PREVIEW_TOOLS_ENV: &str = "TEXRUN_REQUIRE_PREVIEW_TOOLS";

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

/// Tells that `what` is skipped, once per test binary (like the
/// `texrun-preview` real tool tests). Written to the process's stderr
/// directly: `eprintln!` would be captured by the libtest harness and never
/// shown for a passing test. (cargo-nextest captures the whole process
/// output; use `--no-capture` there to see it.)
fn report_skip(what: &'static str, var: &str) {
    static REPORTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut reported = REPORTED.lock().unwrap_or_else(PoisonError::into_inner);
    if reported.contains(&what) {
        return;
    }
    reported.push(what);
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    // Drop cargo's `-<hash>` suffix.
    let binary = binary.rsplit_once('-').map_or(binary.as_str(), |(b, _)| b);
    let _ = writeln!(
        std::io::stderr(),
        "texrun-texlive {binary}: SKIPPED {what} (set {var}=1 to fail instead)"
    );
}

/// Whether TeX Live (latexmk) is available. Panics if it is not and
/// [`REQUIRE_TEXLIVE_ENV`] is `1`.
pub fn texlive_available() -> bool {
    static PROBE: OnceLock<Result<String, String>> = OnceLock::new();
    let probe = PROBE.get_or_init(|| {
        LatexmkEngine::default()
            .probe()
            .map(|info| info.version.unwrap_or_default())
            .map_err(|e| e.to_string())
    });
    match probe {
        Ok(_) => true,
        Err(e) if required(REQUIRE_TEXLIVE_ENV) => {
            panic!("TeX Live is required ({REQUIRE_TEXLIVE_ENV}=1) but not usable: {e}")
        }
        Err(_) => {
            report_skip("all TeX Live tests: latexmk not found", REQUIRE_TEXLIVE_ENV);
            false
        }
    }
}

/// Returns from the calling test unless TeX Live is available.
macro_rules! require_texlive {
    () => {
        if !common::texlive_available() {
            return;
        }
    };
}
pub(crate) use require_texlive;

/// `tests/fixtures/<name>`.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copies `src` recursively into `dst` (regular files and directories only).
pub fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            copy_tree(&entry.path(), &to);
        } else if ty.is_file() {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// A temporary directory whose name has no dot-prefixed component of its
/// own (kpathsea's paranoid mode also refuses those, which would hide the
/// reason a path is refused).
pub fn plain_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("texrun-it-")
        .tempdir()
        .unwrap()
}

/// A project root (a fixture or a staged copy) and the options to compile
/// one of its entrypoints.
pub struct Compile<'a> {
    root: PathBuf,
    entry: &'a str,
    options: CompileOptions,
    config: WorkspaceConfig,
    engine: LatexmkEngine,
    cancel: Option<CancelToken>,
}

impl<'a> Compile<'a> {
    /// Compiles `entry` of the project at `root` with a 60 s timeout.
    pub fn new(root: impl Into<PathBuf>, entry: &'a str) -> Self {
        Self {
            root: root.into(),
            entry,
            options: CompileOptions::default().with_timeout(Duration::from_secs(60)),
            config: WorkspaceConfig::default(),
            engine: LatexmkEngine::default(),
            cancel: None,
        }
    }

    /// Compiles `entry` of `tests/fixtures/<name>`.
    pub fn fixture(name: &str, entry: &'a str) -> Self {
        Self::new(fixture(name), entry)
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options = self.options.with_timeout(timeout);
        self
    }

    pub fn config(mut self, config: WorkspaceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn engine(mut self, engine: LatexmkEngine) -> Self {
        self.engine = engine;
        self
    }

    pub fn cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Creates the workspace without compiling.
    pub fn workspace(&self) -> Workspace {
        let input = ProjectInput::new(&self.root, self.entry).unwrap();
        Workspace::create(&input, self.options.clone(), &self.config).unwrap()
    }

    /// Creates the workspace and compiles.
    pub fn run(self) -> (LatexmkRun, Workspace) {
        let ws = self.workspace();
        let run = self.run_in(&ws);
        (run, ws)
    }

    /// Compiles in an existing workspace.
    pub fn run_in(&self, ws: &Workspace) -> LatexmkRun {
        let mut ctx = ws.context();
        if let Some(cancel) = &self.cancel {
            ctx = ctx.with_cancel(cancel.clone());
        }
        self.engine.run(&ctx, ws.request()).unwrap()
    }
}

/// Everything useful for a failure message.
pub fn describe(run: &LatexmkRun) -> String {
    format!(
        "outcome {:?}, exit {:?}, artifacts {:?}, diagnostics {:#?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        run.result.outcome,
        run.result.exit,
        run.result.artifacts,
        run.result.diagnostics,
        String::from_utf8_lossy(&run.stdout.bytes),
        String::from_utf8_lossy(&run.stderr.bytes),
    )
}

pub fn assert_outcome(run: &LatexmkRun, expected: CompileOutcome) {
    assert_eq!(run.result.outcome, expected, "{}", describe(run));
}

/// The first diagnostic of `kind`, or a panic with the whole run.
pub fn find(run: &LatexmkRun, kind: DiagnosticKind) -> &Diagnostic {
    run.result
        .diagnostics
        .iter()
        .find(|d| d.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} diagnostic: {}", describe(run)))
}

pub fn has(run: &LatexmkRun, kind: DiagnosticKind) -> bool {
    run.result.diagnostics.iter().any(|d| d.kind == kind)
}

pub fn file_of(d: &Diagnostic) -> Option<&str> {
    d.file.as_ref().map(WorkspacePath::as_str)
}

/// Asserts that `d` points at `file`:`line` (workspace-relative).
pub fn assert_location(d: &Diagnostic, file: &str, line: u32) {
    assert_eq!(
        (file_of(d), d.line),
        (Some(file), Some(line)),
        "wrong location: {d:#?}"
    );
}

/// The reported PDF artifact exists in the output directory, has the
/// reported size and starts like a PDF. Returns its host path.
pub fn assert_pdf(run: &LatexmkRun, ws: &Workspace, expected: &str) -> PathBuf {
    let pdf = run
        .result
        .pdf()
        .unwrap_or_else(|| panic!("no PDF artifact: {}", describe(run)));
    assert_eq!(pdf.path.as_str(), expected, "{}", describe(run));
    let path = ws.output_dir().join(pdf.path.as_path());
    let bytes = fs::read(&path).unwrap();
    assert!(
        bytes.starts_with(b"%PDF-"),
        "{} is not a PDF",
        path.display()
    );
    assert_eq!(pdf.size_bytes, Some(bytes.len() as u64));
    path
}

/// A previewer if a preview backend (`mutool` or Poppler) is installed.
/// `None` (with a `SKIPPED` note) otherwise, unless
/// [`REQUIRE_PREVIEW_TOOLS_ENV`] is `1`.
pub fn previewer() -> Option<Previewer> {
    let previewer = Previewer::detect();
    if previewer.toolset().available().is_empty() {
        assert!(
            !required(REQUIRE_PREVIEW_TOOLS_ENV),
            "a preview tool (mutool or pdfinfo + pdftoppm) is required \
             ({REQUIRE_PREVIEW_TOOLS_ENV}=1) but none is installed"
        );
        report_skip(
            "PDF page count and preview checks: no preview tool found",
            REQUIRE_PREVIEW_TOOLS_ENV,
        );
        return None;
    }
    Some(previewer)
}

/// Number of pages of `pdf`, read with `texrun-preview`, or `None` without
/// a preview tool (see [`previewer`]).
pub fn pdf_page_count(pdf: &Path) -> Option<u32> {
    let report = previewer()?
        .inspect(pdf, &PreviewOptions::default())
        .unwrap();
    let info = report
        .pdf
        .unwrap_or_else(|| panic!("PDF metadata not read: {:#?}", report.notices));
    Some(info.page_count)
}

/// Asserts the page count of `pdf` when a preview tool is available.
pub fn assert_page_count(pdf: &Path, expected: u32) {
    if let Some(pages) = pdf_page_count(pdf) {
        assert_eq!(pages, expected, "page count of {}", pdf.display());
    }
}

/// Files below `dir` (recursively, symlinks not followed) whose name
/// starts with `prefix`.
pub fn files_named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            found.push(path.clone());
        }
        if entry.file_type().unwrap().is_dir() {
            found.extend(files_named(&path, prefix));
        }
    }
    found
}

/// Sorted names in `dir`.
pub fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The host `PATH` with `dir` prepended.
pub fn path_with(dir: &Path) -> OsString {
    let mut parts = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(parts).unwrap()
}

/// One process from `ps`.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub stat: String,
    pub comm: String,
    pub args: String,
}

pub fn processes() -> Vec<Proc> {
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
pub fn live_group_members(pgid: u32) -> Vec<Proc> {
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

/// Absolute path of `program` on the host `PATH`.
pub fn which(program: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("`{program}` not found in PATH"))
}
