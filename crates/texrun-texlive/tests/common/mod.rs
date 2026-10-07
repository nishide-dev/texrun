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
//! # Backends
//!
//! [`TEST_BACKEND_ENV`]`=container` runs the tests that use the default
//! engine ([`Compile::new`]) with the container backend
//! ([`ContainerEngine`], image [`SANDBOX_IMAGE_ENV`] or
//! `texrun-engine:latest`) instead of latexmk on the host; TeX Live is then
//! only needed in the image. Tests that depend on the host's TeX Live
//! (wrappers in `PATH`, running pdflatex directly) start with
//! [`host_only!`] and are skipped there. `tests/container.rs` checks what
//! only the container backend guarantees; it runs whenever the backend is
//! usable, and [`REQUIRE_SANDBOX_ENV`]`=1` turns a missing runtime or image
//! into a failure.
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
    CancelToken, CompileContext, CompileOptions, CompileOutcome, CompileRequest, Diagnostic,
    DiagnosticKind, EngineError, EngineInfo, TypesetEngine, WorkspacePath,
};
use texrun_preview::{PreviewContainer, PreviewOptions, Previewer};
use texrun_sandbox::Runtime;
use texrun_texlive::{ContainerConfig, ContainerEngine, LatexmkEngine, LatexmkRun};
use texrun_workspace::{ProjectInput, Workspace, WorkspaceConfig};

/// Set to `1` to fail (instead of skip) tests that need TeX Live.
pub const REQUIRE_TEXLIVE_ENV: &str = "TEXRUN_REQUIRE_TEXLIVE";

/// Set to `1` to fail (instead of skip) checks that need a preview tool
/// (page counts, previews). The same variable as in the `texrun-preview`
/// real tool tests.
pub const REQUIRE_PREVIEW_TOOLS_ENV: &str = "TEXRUN_REQUIRE_PREVIEW_TOOLS";

/// `container` runs the default engine in a container (see the module
/// docs); anything else, or unset, runs latexmk on the host.
pub const TEST_BACKEND_ENV: &str = "TEXRUN_TEST_BACKEND";

/// The image of the container backend in tests (default:
/// [`LOCAL_SANDBOX_IMAGE`]).
pub const SANDBOX_IMAGE_ENV: &str = "TEXRUN_SANDBOX_IMAGE";

/// The image the tests use unless [`SANDBOX_IMAGE_ENV`] is set: a local
/// build of `docker/engine` (`docker build -t texrun-engine:latest
/// docker/engine`), not the published image that texrun uses by default.
pub const LOCAL_SANDBOX_IMAGE: &str = "texrun-engine:latest";

/// Set to `1` to fail (instead of skip) tests that need the container
/// backend (a runtime and the engine image).
pub const REQUIRE_SANDBOX_ENV: &str = "TEXRUN_REQUIRE_SANDBOX";

/// A checkout of the ACL template (acl-org/acl-style-files at the commit
/// pinned in `.github/scripts/fetch-acl-style-files.sh`, #71). Its files
/// have no license that allows bundling them as a fixture, so the test that
/// compiles it ([`acl_style_files`]) is skipped unless this is set.
pub const ACL_STYLE_FILES_ENV: &str = "TEXRUN_ACL_STYLE_FILES";

/// The directory of [`ACL_STYLE_FILES_ENV`], or `None` (with a `SKIPPED`
/// note) when it is not set. Panics if it is set but is not a checkout of
/// the template.
pub fn acl_style_files() -> Option<PathBuf> {
    match std::env::var_os(ACL_STYLE_FILES_ENV) {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            assert!(
                dir.join("acl_latex.tex").is_file() && dir.join("acl.sty").is_file(),
                "{ACL_STYLE_FILES_ENV}={} is not a checkout of acl-org/acl-style-files",
                dir.display()
            );
            Some(dir)
        }
        _ => {
            report_skip_with(
                "the ACL template test: no checkout of acl-org/acl-style-files",
                &format!(
                    "set {ACL_STYLE_FILES_ENV} to the directory of \
                     .github/scripts/fetch-acl-style-files.sh"
                ),
            );
            None
        }
    }
}

/// Whether the default engine of the tests is the container backend.
pub fn container_backend() -> bool {
    std::env::var_os(TEST_BACKEND_ENV).is_some_and(|v| v == "container")
}

/// A container engine for the tests' image.
pub fn container_engine(config: ContainerConfig) -> ContainerEngine {
    let config = match std::env::var(SANDBOX_IMAGE_ENV) {
        Ok(image) if !image.is_empty() => config.with_image(image),
        _ => config.with_image(LOCAL_SANDBOX_IMAGE),
    };
    ContainerEngine::new(config)
}

/// The engine a test compiles with.
#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "one value per compile")]
pub enum TestEngine {
    /// latexmk on the host.
    Host(LatexmkEngine),
    /// latexmk in a container.
    Container(ContainerEngine),
}

impl TestEngine {
    /// The default engine of [`TEST_BACKEND_ENV`].
    pub fn default_backend() -> Self {
        if container_backend() {
            Self::Container(container_engine(ContainerConfig::default()))
        } else {
            Self::Host(LatexmkEngine::default())
        }
    }

    pub fn run(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<LatexmkRun, EngineError> {
        match self {
            Self::Host(engine) => engine.run(ctx, request),
            Self::Container(engine) => engine.run(ctx, request),
        }
    }

    pub fn probe(&self) -> Result<EngineInfo, EngineError> {
        match self {
            Self::Host(engine) => engine.probe(),
            Self::Container(engine) => engine.probe(),
        }
    }
}

impl From<LatexmkEngine> for TestEngine {
    fn from(engine: LatexmkEngine) -> Self {
        Self::Host(engine)
    }
}

impl From<ContainerEngine> for TestEngine {
    fn from(engine: ContainerEngine) -> Self {
        Self::Container(engine)
    }
}

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

/// Tells that `what` is skipped, once per test binary (like the
/// `texrun-preview` real tool tests). Written to the process's stderr
/// directly: `eprintln!` would be captured by the libtest harness and never
/// shown for a passing test. (cargo-nextest captures the whole process
/// output; use `--no-capture` there to see it.)
fn report_skip(what: &'static str, var: &str) {
    report_skip_with(what, &format!("set {var}=1 to fail instead"));
}

/// [`report_skip`] with another hint in the parentheses.
fn report_skip_with(what: &'static str, hint: &str) {
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
        "texrun-texlive {binary}: SKIPPED {what} ({hint})"
    );
}

/// Whether TeX Live (latexmk) is available to the default engine (on the
/// host, or in the image with the container backend). Panics if it is not
/// and [`REQUIRE_TEXLIVE_ENV`] is `1`.
pub fn texlive_available() -> bool {
    static PROBE: OnceLock<Result<String, String>> = OnceLock::new();
    let probe = PROBE.get_or_init(|| {
        TestEngine::default_backend()
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
#[allow(unused_macros)]
macro_rules! require_texlive {
    () => {
        if !common::texlive_available() {
            return;
        }
    };
}
#[allow(unused_imports)]
pub(crate) use require_texlive;

/// Whether the container backend can be used (a runtime and the image).
/// Panics if it cannot and [`REQUIRE_SANDBOX_ENV`] is `1`.
pub fn sandbox_available() -> bool {
    static PROBE: OnceLock<Result<String, String>> = OnceLock::new();
    let probe = PROBE.get_or_init(|| {
        container_engine(ContainerConfig::default())
            .probe()
            .map(|info| info.version.unwrap_or_default())
            .map_err(|e| e.to_string())
    });
    match probe {
        Ok(_) => true,
        Err(e) if required(REQUIRE_SANDBOX_ENV) => {
            panic!(
                "the container backend is required ({REQUIRE_SANDBOX_ENV}=1) but not usable: {e}"
            )
        }
        Err(_) => {
            report_skip(
                "container backend tests: no container runtime or engine image",
                REQUIRE_SANDBOX_ENV,
            );
            false
        }
    }
}

/// Returns from the calling test unless the container backend is usable.
#[allow(unused_macros)]
macro_rules! require_sandbox {
    () => {
        if !common::sandbox_available() {
            return;
        }
    };
}
#[allow(unused_imports)]
pub(crate) use require_sandbox;

/// Tells (once per test binary) that tests were skipped because they need
/// TeX Live on the host.
pub fn report_host_only() {
    report_skip_with(
        "host-only tests: they need TeX Live on the host",
        &format!("they run without {TEST_BACKEND_ENV}=container"),
    );
}

/// Returns from the calling test when the default engine is the container
/// backend: the test needs TeX Live on the host itself.
#[allow(unused_macros)]
macro_rules! host_only {
    () => {
        if common::container_backend() {
            common::report_host_only();
            return;
        }
    };
}
#[allow(unused_imports)]
pub(crate) use host_only;

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
    engine: TestEngine,
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
            engine: TestEngine::default_backend(),
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

    pub fn engine(mut self, engine: impl Into<TestEngine>) -> Self {
        self.engine = engine.into();
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

/// A previewer: with the container backend, one that runs the tools in a
/// container of the tests' image (as `texrun compile --backend container`
/// does); otherwise one for the preview backends (`mutool` or Poppler)
/// installed on the host, or `None` (with a `SKIPPED` note) if there is
/// none, unless [`REQUIRE_PREVIEW_TOOLS_ENV`] is `1`.
pub fn previewer() -> Option<Previewer> {
    if container_backend() {
        return Some(Previewer::in_container(preview_container()));
    }
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

/// The container of the container backend's previews: the tests' image
/// ([`SANDBOX_IMAGE_ENV`]) with the detected runtime. Only called once the
/// container backend was found usable ([`texlive_available`]).
fn preview_container() -> PreviewContainer {
    static CONTAINER: OnceLock<PreviewContainer> = OnceLock::new();
    CONTAINER
        .get_or_init(|| {
            let image = match std::env::var(SANDBOX_IMAGE_ENV) {
                Ok(image) if !image.is_empty() => image,
                _ => LOCAL_SANDBOX_IMAGE.to_owned(),
            };
            let runtime = Runtime::detect(None).expect("container runtime");
            let id = runtime.image_id(&image).expect("engine image");
            PreviewContainer::new(runtime, id)
        })
        .clone()
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
