//! The latexmk engine.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tempfile::TempDir;
use texrun_core::{
    Artifact, ArtifactKind, CompileContext, CompileOutcome, CompileRequest, CompileResult,
    Diagnostic, DiagnosticKind, EngineError, EngineInfo, ProcessExit, Severity, TypesetEngine,
    WorkspacePath, WorkspaceRoot,
};
use texrun_latex_log::LogParser;

use crate::command::{self, MAX_PRINT_LINE};
use crate::layout::{self, HOME_DIR};
use crate::names::{check_host_path, check_name};
use crate::process::{self, CapturedOutput, Job, Limits, StopReason};
use crate::rc::{self, RcOptions};

/// Engine identifier reported in [`EngineInfo::name`].
pub const ENGINE_NAME: &str = "texlive";

/// Name of the latexmk executable searched for in `PATH`.
pub const LATEXMK_PROGRAM: &str = "latexmk";

/// Timeout applied when the request has none (docs/security.md §3.1).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout of `latexmk -v` in [`TypesetEngine::probe`].
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// At most this much of the main log is given to the log parser. A longer
/// log is parsed from its last this-many bytes, where TeX reports the error
/// that stopped it (`-halt-on-error`).
pub const MAX_PARSED_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// File name of the rc inside its temporary directory.
const RC_FILE_NAME: &str = "texrun.latexmkrc";

/// Configuration of a [`LatexmkEngine`].
///
/// `#[non_exhaustive]`: construct with [`LatexmkConfig::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LatexmkConfig {
    /// Use this latexmk executable instead of searching `PATH`. A relative
    /// path is resolved against texrun's working directory; the resolved
    /// absolute path (symlinks resolved) is what gets run.
    pub latexmk: Option<PathBuf>,
    /// The `PATH` used to find latexmk and passed to it (after dropping
    /// empty and relative entries). `None`: the host `PATH`.
    pub search_path: Option<OsString>,
    /// Directory in which the per-compile rc directory is created. `None`:
    /// the system temporary directory. Must be outside the workspace.
    pub rc_parent: Option<PathBuf>,
    /// Timeout used when [`CompileOptions::timeout`](texrun_core::CompileOptions::timeout)
    /// is `None`.
    pub default_timeout: Duration,
    /// Size limits.
    pub limits: Limits,
    /// Sets `SOURCE_DATE_EPOCH` (and `FORCE_SOURCE_DATE=1`) for reproducible
    /// PDF dates (docs/security.md §3.9). Off by default.
    pub source_date_epoch: Option<i64>,
}

impl Default for LatexmkConfig {
    fn default() -> Self {
        Self {
            latexmk: None,
            search_path: None,
            rc_parent: None,
            default_timeout: DEFAULT_TIMEOUT,
            limits: Limits::default(),
            source_date_epoch: None,
        }
    }
}

impl LatexmkConfig {
    /// Sets [`LatexmkConfig::latexmk`].
    #[must_use]
    pub fn with_latexmk(mut self, path: impl Into<PathBuf>) -> Self {
        self.latexmk = Some(path.into());
        self
    }

    /// Sets [`LatexmkConfig::search_path`].
    #[must_use]
    pub fn with_search_path(mut self, path: impl Into<OsString>) -> Self {
        self.search_path = Some(path.into());
        self
    }

    /// Sets [`LatexmkConfig::rc_parent`].
    #[must_use]
    pub fn with_rc_parent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.rc_parent = Some(dir.into());
        self
    }

    /// Sets [`LatexmkConfig::default_timeout`].
    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets [`LatexmkConfig::limits`].
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets [`LatexmkConfig::source_date_epoch`].
    #[must_use]
    pub fn with_source_date_epoch(mut self, epoch: Option<i64>) -> Self {
        self.source_date_epoch = epoch;
        self
    }
}

/// Everything a compile produced: the [`CompileResult`] plus data that is
/// not part of the backend-independent model.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LatexmkRun {
    /// The result returned by [`TypesetEngine::compile`].
    pub result: CompileResult,
    /// Head of latexmk's stdout (the console output of all passes).
    pub stdout: CapturedOutput,
    /// Head of latexmk's stderr.
    pub stderr: CapturedOutput,
    /// PID of latexmk, which was also the process group ID; `0` if latexmk
    /// was not started (cancelled before the start).
    ///
    /// For diagnostics (logs, tests) only. The process has been reaped when
    /// this value is returned, so the PID may already belong to an unrelated
    /// process: never send signals to it.
    pub pid: u32,
}

/// TeX Live + latexmk engine (docs/security.md §3).
///
/// Discovery of latexmk happens on every [`TypesetEngine::probe`] /
/// compile, so a missing installation is reported as
/// [`EngineError::Unavailable`] at that point, not on construction.
///
/// The latexmk version is only known after a successful
/// [`TypesetEngine::probe`]; until then [`TypesetEngine::info`] (and so
/// [`CompileResult::engine`]) has no version. Callers that report the version
/// (e.g. the CLI) should call `probe()` once before compiling.
#[derive(Debug)]
pub struct LatexmkEngine {
    config: LatexmkConfig,
    /// latexmk version from the last successful probe.
    version: OnceLock<String>,
}

impl Default for LatexmkEngine {
    fn default() -> Self {
        Self::new(LatexmkConfig::default())
    }
}

impl LatexmkEngine {
    /// Creates an engine. Does not touch the filesystem.
    pub fn new(config: LatexmkConfig) -> Self {
        Self {
            config,
            version: OnceLock::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &LatexmkConfig {
        &self.config
    }

    /// The sanitized `PATH` for discovery and for the child.
    fn child_path(&self) -> OsString {
        let raw = self
            .config
            .search_path
            .clone()
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        command::sanitize_path(&raw)
    }

    /// Finds the latexmk executable.
    pub fn locate(&self) -> Result<PathBuf, EngineError> {
        if let Some(explicit) = &self.config.latexmk {
            // Resolve now (a relative path against texrun's working
            // directory): spawned with the workspace as working directory, a
            // relative program path would name a file inside the workspace.
            let resolved = fs::canonicalize(explicit)
                .map_err(|e| unavailable(format!("cannot resolve {}: {e}", explicit.display())))?;
            return if command::is_executable_file(&resolved) {
                Ok(resolved)
            } else {
                Err(unavailable(format!(
                    "{} is not an executable file",
                    explicit.display()
                )))
            };
        }
        command::find_in_path(LATEXMK_PROGRAM, &self.child_path()).ok_or_else(|| {
            unavailable(format!(
                "`{LATEXMK_PROGRAM}` was not found in PATH (install TeX Live with latexmk, or \
                 configure the path to latexmk)"
            ))
        })
    }

    /// Compiles like [`TypesetEngine::compile`] and also returns the
    /// captured console output.
    ///
    /// An existing `<stem>.pdf` / `<stem>.log` in the output directory (e.g.
    /// left from an earlier compile in the same workspace) is removed before
    /// latexmk starts, so the reported artifacts are always from this run.
    /// Other files in the output directory are left alone, and latexmk may
    /// reuse them (e.g. `.aux`).
    pub fn run(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<LatexmkRun, EngineError> {
        request.validate()?;
        let plan = Plan::new(ctx.workspace, request)?;
        let latexmk = self.locate()?;

        if ctx.cancel.is_cancelled() {
            let result = CompileResult::new(CompileOutcome::Cancelled, self.info(), Duration::ZERO);
            return Ok(LatexmkRun {
                result,
                stdout: CapturedOutput::default(),
                stderr: CapturedOutput::default(),
                pid: 0,
            });
        }

        plan.prepare_dirs()?;
        let (rc_dir, rc_path) = self.write_rc(&plan.root)?;

        let limits = self.config.limits;
        let job = Job {
            program: &latexmk,
            args: command::latexmk_args(&rc_path, &plan.output_dir, &plan.entry_arg),
            cwd: &plan.cwd,
            env: command::child_env(
                &self.child_path(),
                &plan.home,
                self.config.source_date_epoch,
            ),
            timeout: Some(
                request
                    .options
                    .timeout
                    .unwrap_or(self.config.default_timeout),
            ),
            cancel: Some(&ctx.cancel),
            size_dirs: vec![plan.output_dir.as_path(), plan.home.as_path()],
            limits,
            file_size_gate: process::FILE_SIZE_GATE_SUPPORTED,
        };
        let finished = process::run(&job);
        // The rc is not needed any more, whatever happened.
        let _ = rc_dir.close();
        let finished = finished?;

        // A file that hit RLIMIT_FSIZE stops the writer without the poll loop
        // noticing; check the sizes once more.
        let stop = finished.stop.clone().or_else(|| {
            process::check_output_size(&[plan.output_dir.as_path(), plan.home.as_path()], &limits)
        });

        let artifacts = plan.collect_artifacts();
        let pdf_ok = artifacts.iter().any(|a| a.kind == ArtifactKind::Pdf);
        let mut diagnostics = Vec::new();
        let outcome = match &stop {
            Some(StopReason::TimedOut) => CompileOutcome::TimedOut,
            Some(StopReason::Cancelled) => CompileOutcome::Cancelled,
            Some(StopReason::OutputLimit(message)) => {
                diagnostics.push(Diagnostic::new(
                    Severity::Error,
                    DiagnosticKind::Other,
                    message.clone(),
                ));
                CompileOutcome::Failed
            }
            None if finished.status.success() && pdf_ok => CompileOutcome::Succeeded,
            None => {
                if finished.status.success() {
                    diagnostics.push(Diagnostic::new(
                        Severity::Error,
                        DiagnosticKind::Other,
                        "latexmk finished without producing a PDF",
                    ));
                }
                CompileOutcome::Failed
            }
        };
        diagnostics.extend(plan.parse_log());

        let mut result = CompileResult::new(outcome, self.info(), finished.elapsed);
        result.exit = Some(ProcessExit::from(finished.status));
        result.diagnostics = diagnostics;
        result.artifacts = artifacts;
        Ok(LatexmkRun {
            result,
            stdout: finished.stdout,
            stderr: finished.stderr,
            pid: finished.pid,
        })
    }

    /// Writes the texrun rc into a new temporary directory, which must be
    /// outside the workspace (`workspace_root`, canonical). Returns the
    /// directory, removed when dropped, and the rc path.
    fn write_rc(&self, workspace_root: &Path) -> Result<(TempDir, PathBuf), EngineError> {
        let parent = self
            .config
            .rc_parent
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::Builder::new()
            .prefix("texrun-rc-")
            .tempdir_in(&parent)
            .map_err(io_error(format!(
                "creating the latexmk rc directory in {}",
                parent.display()
            )))?;
        let real = fs::canonicalize(dir.path())
            .map_err(io_error(format!("resolving {}", dir.path().display())))?;
        if real.starts_with(workspace_root) {
            return Err(unavailable(format!(
                "the latexmk rc directory {} must be outside the workspace",
                real.display()
            )));
        }
        let path = real.join(RC_FILE_NAME);
        check_host_path("latexmk rc path", &path)?;
        let rc = rc::render(RcOptions {
            stdin_gate: process::FILE_SIZE_GATE_SUPPORTED,
        });
        fs::write(&path, rc).map_err(io_error(format!("writing {}", path.display())))?;
        Ok((dir, path))
    }
}

impl TypesetEngine for LatexmkEngine {
    fn info(&self) -> EngineInfo {
        let info = EngineInfo::new(ENGINE_NAME);
        match self.version.get() {
            Some(v) => info.with_version(v.clone()),
            None => info,
        }
    }

    fn probe(&self) -> Result<EngineInfo, EngineError> {
        let latexmk = self.locate()?;
        let scratch = tempfile::Builder::new()
            .prefix("texrun-probe-")
            .tempdir()
            .map_err(io_error("creating a directory for `latexmk -v`".to_owned()))?;
        let job = Job {
            program: &latexmk,
            args: vec!["-norc".into(), "-v".into()],
            cwd: scratch.path(),
            env: command::child_env(&self.child_path(), scratch.path(), None),
            timeout: Some(PROBE_TIMEOUT),
            cancel: None,
            size_dirs: Vec::new(),
            limits: self.config.limits,
            file_size_gate: false,
        };
        let finished = process::run(&job)?;
        let stdout = String::from_utf8_lossy(&finished.stdout.bytes);
        let version = match (finished.stop, parse_version(&stdout)) {
            (None, Some(v)) if finished.status.success() => v,
            _ => {
                return Err(unavailable(format!(
                    "`{} -v` did not report a version (exit: {:?})",
                    latexmk.display(),
                    ProcessExit::from(finished.status)
                )));
            }
        };
        let _ = self.version.set(format!("latexmk {version}"));
        Ok(self.info())
    }

    fn compile(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<CompileResult, EngineError> {
        self.run(ctx, request).map(|run| run.result)
    }
}

/// Extracts `4.86` from `Latexmk, John Collins, 11 Dec. 2024. Version 4.86`.
pub(crate) fn parse_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Latexmk,")?;
        let (_, version) = rest.rsplit_once("Version ")?;
        let version = version.trim();
        (!version.is_empty()).then(|| version.to_owned())
    })
}

fn unavailable(reason: String) -> EngineError {
    EngineError::Unavailable {
        engine: ENGINE_NAME.to_owned(),
        reason,
    }
}

fn io_error(context: String) -> impl FnOnce(io::Error) -> EngineError {
    move |source| EngineError::Io { context, source }
}

/// Host paths and arguments for one compile, derived and checked from the
/// request.
#[derive(Debug)]
struct Plan {
    /// Canonical workspace root.
    root: PathBuf,
    /// Entrypoint directory relative to the root (`None`: the root).
    entry_dir_rel: Option<WorkspacePath>,
    output_dir_rel: WorkspacePath,
    /// Working directory of latexmk: the entrypoint's directory.
    cwd: PathBuf,
    /// Absolute output directory.
    output_dir: PathBuf,
    home: PathBuf,
    /// The entrypoint file name as an argument (`./name.tex`).
    entry_arg: String,
    /// Job name (file stem of the entrypoint).
    stem: String,
}

impl Plan {
    fn new(workspace: &WorkspaceRoot, request: &CompileRequest) -> Result<Self, EngineError> {
        let entry = &request.entrypoint;
        let output_dir_rel = request.options.output_dir.clone();
        check_name("entrypoint", entry.as_str())?;
        check_name("output directory", output_dir_rel.as_str())?;

        let home_rel = WorkspacePath::new(HOME_DIR).expect("valid constant");
        if output_dir_rel.starts_with(&home_rel) || home_rel.starts_with(&output_dir_rel) {
            return Err(EngineError::InvalidRequest(format!(
                "output directory `{output_dir_rel}` overlaps texrun's `{HOME_DIR}`"
            )));
        }

        let root = fs::canonicalize(workspace.path()).map_err(io_error(format!(
            "resolving the workspace {}",
            workspace.path().display()
        )))?;
        check_host_path("workspace directory", &root)?;

        let entry_host = root.join(entry.as_path());
        if !entry_host.is_file() {
            return Err(EngineError::InvalidRequest(format!(
                "entrypoint `{entry}` is not a file in the workspace"
            )));
        }
        let entry_dir_rel = entry
            .as_str()
            .rsplit_once('/')
            .map(|(dir, _)| WorkspacePath::new(dir).expect("parent of a valid path"));
        let cwd = match &entry_dir_rel {
            Some(dir) => root.join(dir.as_path()),
            None => root.clone(),
        };
        // Symlinks inside the workspace may only point inside it (#4); make
        // sure the directory latexmk runs in really is inside.
        let cwd =
            fs::canonicalize(&cwd).map_err(io_error(format!("resolving {}", cwd.display())))?;
        if !cwd.starts_with(&root) {
            return Err(EngineError::InvalidRequest(format!(
                "the directory of entrypoint `{entry}` resolves outside the workspace"
            )));
        }
        check_host_path("entrypoint directory", &cwd)?;

        let output_dir = root.join(output_dir_rel.as_path());
        check_host_path("output directory", &output_dir)?;
        let home = root.join(HOME_DIR);

        let file_name = WorkspacePath::new(entry.file_name()).expect("file name of a valid path");
        Ok(Self {
            entry_arg: file_name.to_cli_arg(),
            stem: entry.file_stem().to_owned(),
            root,
            entry_dir_rel,
            output_dir_rel,
            cwd,
            output_dir,
            home,
        })
    }

    /// Creates `HOME` and the output directory and mirrors the entrypoint's
    /// directory tree into the output directory.
    fn prepare_dirs(&self) -> Result<(), EngineError> {
        for dir in [&self.home, &self.output_dir] {
            layout::ensure_dir(dir).map_err(io_error(format!("creating {}", dir.display())))?;
            let real =
                fs::canonicalize(dir).map_err(io_error(format!("resolving {}", dir.display())))?;
            if !real.starts_with(&self.root) {
                return Err(EngineError::InvalidRequest(format!(
                    "{} resolves outside the workspace",
                    dir.display()
                )));
            }
        }
        // Stale artifacts would be reported as this run's output.
        for ext in ["pdf", "log"] {
            let path = self.output_dir.join(format!("{}.{ext}", self.stem));
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Ok(meta) if !meta.is_dir() => fs::remove_file(&path)
                    .map_err(io_error(format!("removing the stale {}", path.display())))?,
                Ok(_) => {
                    return Err(EngineError::InvalidRequest(format!(
                        "{} is a directory",
                        path.display()
                    )));
                }
                Err(e) => return Err(io_error(format!("inspecting {}", path.display()))(e)),
            }
        }
        layout::mirror_subdirs(
            &self.cwd,
            self.entry_dir_rel.as_ref(),
            &self.output_dir_rel,
            &self.output_dir,
        )
        .map_err(io_error(
            "mirroring the source directories into the output directory".to_owned(),
        ))
    }

    /// The PDF and the main log, if latexmk produced them as regular files.
    /// `.fls`, `.fdb_latexmk`, `.aux` etc. are never reported (§3.9).
    fn collect_artifacts(&self) -> Vec<Artifact> {
        [(ArtifactKind::Pdf, "pdf"), (ArtifactKind::Log, "log")]
            .into_iter()
            .filter_map(|(kind, ext)| {
                let name = WorkspacePath::new(&format!("{}.{ext}", self.stem)).ok()?;
                let meta = fs::symlink_metadata(self.output_dir.join(name.as_path())).ok()?;
                meta.is_file()
                    .then(|| Artifact::new(kind, name).with_size_bytes(meta.len()))
            })
            .collect()
    }

    /// Parses the main log into diagnostics with workspace-relative files.
    fn parse_log(&self) -> Vec<Diagnostic> {
        let mut buf = Vec::new();
        let path = self.output_dir.join(format!("{}.log", self.stem));
        let Ok(truncated) = read_log(&path, &mut buf) else {
            return Vec::new();
        };
        // TeX ran in the entrypoint's directory, so the log names files
        // relative to it; the parser wants TeX's working directory as root.
        let Ok(tex_cwd) = WorkspaceRoot::new(&self.cwd) else {
            return Vec::new();
        };
        let parsed = LogParser::new()
            .with_workspace_root(&tex_cwd)
            .with_max_print_line(MAX_PRINT_LINE)
            .parse(&buf);
        let mut diagnostics: Vec<Diagnostic> = parsed
            .diagnostics
            .into_iter()
            .map(|mut d| {
                if let Some(dir) = &self.entry_dir_rel {
                    d.file = d.file.map(|file| dir.join(&file));
                }
                d
            })
            .collect();
        if truncated {
            diagnostics.push(Diagnostic::new(
                Severity::Info,
                DiagnosticKind::Other,
                format!(
                    "the log is larger than {MAX_PARSED_LOG_BYTES} bytes; only its end was analyzed"
                ),
            ));
        }
        diagnostics
    }
}

/// Reads the log into `buf`: all of it, or its last
/// [`MAX_PARSED_LOG_BYTES`] bytes. Returns whether it was cut.
fn read_log(path: &Path, buf: &mut Vec<u8>) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let truncated = len > MAX_PARSED_LOG_BYTES;
    if truncated {
        file.seek(SeekFrom::Start(len - MAX_PARSED_LOG_BYTES))?;
    }
    buf.clear();
    file.take(MAX_PARSED_LOG_BYTES).read_to_end(buf)?;
    Ok(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_latexmk_version() {
        assert_eq!(
            parse_version("Latexmk, John Collins, 11 Dec. 2024. Version 4.86\n").as_deref(),
            Some("4.86")
        );
        assert_eq!(
            parse_version("\nLatexmk, John Collins, 7 Apr. 2021. Version 4.74b\n").as_deref(),
            Some("4.74b")
        );
        assert_eq!(parse_version("something else"), None);
    }

    fn workspace_with(files: &[&str]) -> (tempfile::TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "").unwrap();
        }
        let root = WorkspaceRoot::new(fs::canonicalize(dir.path()).unwrap()).unwrap();
        (dir, root)
    }

    fn request(entry: &str) -> CompileRequest {
        CompileRequest::new(WorkspacePath::new(entry).unwrap())
    }

    #[test]
    fn plan_for_root_entrypoint() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let plan = Plan::new(&root, &request("main.tex")).unwrap();
        assert_eq!(plan.cwd, root.path());
        assert_eq!(plan.output_dir, root.path().join(".texrun/out"));
        assert_eq!(plan.home, root.path().join(".texrun/home"));
        assert_eq!(plan.entry_arg, "./main.tex");
        assert_eq!(plan.stem, "main");
        assert!(plan.entry_dir_rel.is_none());
    }

    #[test]
    fn plan_for_subdirectory_and_dash_entrypoint() {
        let (_dir, root) = workspace_with(&["src/-draft.tex"]);
        let plan = Plan::new(&root, &request("src/-draft.tex")).unwrap();
        assert_eq!(plan.cwd, root.path().join("src"));
        assert_eq!(plan.entry_arg, "./-draft.tex");
        assert_eq!(plan.stem, "-draft");
        assert_eq!(plan.entry_dir_rel.unwrap().as_str(), "src");
        // Output stays at the workspace root, as an absolute path.
        assert_eq!(plan.output_dir, root.path().join(".texrun/out"));
    }

    #[test]
    fn plan_rejects_bad_names_and_missing_entrypoints() {
        let (_dir, root) = workspace_with(&["a$b.tex", "main.tex"]);
        for entry in ["a$b.tex", "missing.tex"] {
            let err = Plan::new(&root, &request(entry)).unwrap_err();
            assert!(
                matches!(err, EngineError::InvalidRequest(_)),
                "{entry}: {err:?}"
            );
        }
        let req = request("main.tex").with_options(
            texrun_core::CompileOptions::default()
                .with_output_dir(WorkspacePath::new("out`x").unwrap()),
        );
        assert!(matches!(
            Plan::new(&root, &req),
            Err(EngineError::InvalidRequest(_))
        ));
        let req = request("main.tex").with_options(
            texrun_core::CompileOptions::default()
                .with_output_dir(WorkspacePath::new(".texrun/home/out").unwrap()),
        );
        assert!(matches!(
            Plan::new(&root, &req),
            Err(EngineError::InvalidRequest(_))
        ));
    }

    #[test]
    fn artifacts_are_pdf_and_log_only() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let plan = Plan::new(&root, &request("main.tex")).unwrap();
        plan.prepare_dirs().unwrap();
        for ext in ["pdf", "log", "aux", "fls", "fdb_latexmk"] {
            fs::write(plan.output_dir.join(format!("main.{ext}")), "x").unwrap();
        }
        let artifacts = plan.collect_artifacts();
        let names: Vec<_> = artifacts.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(names, ["main.pdf", "main.log"]);
        assert_eq!(artifacts[0].size_bytes, Some(1));
    }

    #[test]
    fn log_diagnostics_are_workspace_relative() {
        let (_dir, root) = workspace_with(&["src/main.tex", "src/chapters/intro.tex"]);
        let plan = Plan::new(&root, &request("src/main.tex")).unwrap();
        plan.prepare_dirs().unwrap();
        fs::write(
            plan.output_dir.join("main.log"),
            "(./main.tex (./chapters/intro.tex\n./chapters/intro.tex:3: Undefined control sequence.\nl.3 \\foo\n",
        )
        .unwrap();
        let diagnostics = plan.parse_log();
        let d = &diagnostics[0];
        assert_eq!(d.kind, DiagnosticKind::UndefinedControlSequence);
        assert_eq!(d.file.as_ref().unwrap().as_str(), "src/chapters/intro.tex");
        assert_eq!(d.line, Some(3));
    }

    #[test]
    fn log_diagnostics_of_a_root_entrypoint_keep_their_file() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let plan = Plan::new(&root, &request("main.tex")).unwrap();
        plan.prepare_dirs().unwrap();
        fs::write(
            plan.output_dir.join("main.log"),
            "(./main.tex\n./main.tex:4: Undefined control sequence.\nl.4 \\foo\n",
        )
        .unwrap();
        let diagnostics = plan.parse_log();
        assert_eq!(diagnostics[0].file.as_ref().unwrap().as_str(), "main.tex");
    }

    #[test]
    fn long_logs_are_parsed_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut data = vec![b'x'; usize::try_from(MAX_PARSED_LOG_BYTES).unwrap() + 10];
        data.extend_from_slice(b"\nEND");
        fs::write(&path, &data).unwrap();
        let mut buf = Vec::new();
        assert!(read_log(&path, &mut buf).unwrap());
        assert_eq!(buf.len() as u64, MAX_PARSED_LOG_BYTES);
        assert!(buf.ends_with(b"\nEND"));
    }

    #[test]
    fn missing_latexmk_is_unavailable() {
        let empty = tempfile::tempdir().unwrap();
        let engine = LatexmkEngine::new(LatexmkConfig::default().with_search_path(empty.path()));
        assert!(matches!(
            engine.probe(),
            Err(EngineError::Unavailable { .. })
        ));
        let engine =
            LatexmkEngine::new(LatexmkConfig::default().with_latexmk(empty.path().join("latexmk")));
        assert!(matches!(
            engine.locate(),
            Err(EngineError::Unavailable { .. })
        ));
        // compile reports it too, after validating the request.
        let (_dir, root) = workspace_with(&["main.tex"]);
        let ctx = CompileContext::new(&root);
        let err = engine.compile(&ctx, &request("main.tex")).unwrap_err();
        assert!(matches!(err, EngineError::Unavailable { .. }), "{err:?}");
    }

    #[test]
    fn invalid_request_is_rejected_before_discovery() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let ctx = CompileContext::new(&root);
        let engine = LatexmkEngine::new(LatexmkConfig::default().with_search_path(""));
        let req = request("main.tex")
            .with_options(texrun_core::CompileOptions::default().with_timeout(Duration::ZERO));
        assert!(matches!(
            engine.compile(&ctx, &req),
            Err(EngineError::InvalidRequest(_))
        ));
    }

    /// Without the start token, latexmk stops inside the rc before running
    /// or writing anything.
    ///
    /// Needs TeX Live; skipped without it unless `TEXRUN_REQUIRE_TEXLIVE=1`
    /// (like the integration tests, see `tests/common/mod.rs`).
    #[test]
    fn gated_rc_does_nothing_without_the_start_token() {
        use std::io::Write as _;

        let engine = LatexmkEngine::default();
        let latexmk = match engine.locate() {
            Ok(latexmk) => latexmk,
            Err(e) => {
                assert!(
                    std::env::var_os("TEXRUN_REQUIRE_TEXLIVE").is_none_or(|v| v != "1"),
                    "TeX Live is required (TEXRUN_REQUIRE_TEXLIVE=1): {e}"
                );
                let _ = writeln!(
                    std::io::stderr(),
                    "texrun-texlive unit tests: SKIPPED gated_rc_does_nothing_without_the_start_token: \
                     latexmk not found (set TEXRUN_REQUIRE_TEXLIVE=1 to fail instead)"
                );
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.tex"),
            "\\documentclass{article}\\begin{document}x\\end{document}\n",
        )
        .unwrap();
        let rc = dir.path().join("rc");
        fs::write(&rc, rc::render(RcOptions { stdin_gate: true })).unwrap();
        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        let status = std::process::Command::new(latexmk)
            .args(command::latexmk_args(&rc, &out, "./main.tex"))
            .current_dir(dir.path())
            .env_clear()
            .envs(command::child_env(&engine.child_path(), dir.path(), None).vars())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(rc::GATE_EXIT_CODE));
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn explicit_latexmk_is_resolved_to_an_absolute_path() {
        use std::os::unix::fs::PermissionsExt;

        let bin = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(bin.path()).unwrap().join("latexmk");
        fs::write(&real, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

        // The same file, named relative to texrun's working directory.
        let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(real.strip_prefix("/").unwrap());
        assert!(relative.is_relative());

        let engine = LatexmkEngine::new(LatexmkConfig::default().with_latexmk(&relative));
        let located = engine.locate().unwrap();
        assert!(located.is_absolute());
        assert_eq!(located, real);

        // A relative path that does not exist from texrun's working
        // directory is unavailable, whatever exists in a workspace.
        let engine =
            LatexmkEngine::new(LatexmkConfig::default().with_latexmk("texrun-no-such-dir/latexmk"));
        assert!(matches!(
            engine.locate(),
            Err(EngineError::Unavailable { .. })
        ));
    }

    #[test]
    fn stale_pdf_and_log_are_removed_before_the_run() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let plan = Plan::new(&root, &request("main.tex")).unwrap();
        fs::create_dir_all(&plan.output_dir).unwrap();
        for ext in ["pdf", "log", "aux"] {
            fs::write(plan.output_dir.join(format!("main.{ext}")), "old").unwrap();
        }
        plan.prepare_dirs().unwrap();
        assert!(!plan.output_dir.join("main.pdf").exists());
        assert!(!plan.output_dir.join("main.log").exists());
        assert!(plan.output_dir.join("main.aux").exists());
        assert!(plan.collect_artifacts().is_empty());
    }

    #[test]
    fn rc_is_written_outside_the_workspace_and_removed() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        let engine = LatexmkEngine::default();
        let (rc_dir, rc_path) = engine.write_rc(root.path()).unwrap();
        assert!(!rc_path.starts_with(root.path()));
        let text = fs::read_to_string(&rc_path).unwrap();
        assert!(text.contains("sub texrun_run"));
        let dir = rc_dir.path().to_path_buf();
        rc_dir.close().unwrap();
        assert!(!dir.exists());

        // An rc parent inside the workspace is refused.
        let engine = LatexmkEngine::new(LatexmkConfig::default().with_rc_parent(root.path()));
        assert!(matches!(
            engine.write_rc(root.path()),
            Err(EngineError::Unavailable { .. })
        ));
    }

    #[test]
    fn cancelled_before_start_runs_nothing() {
        let (_dir, root) = workspace_with(&["main.tex"]);
        // A fake latexmk that would fail loudly if it were run.
        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("latexmk");
        fs::write(&fake, "#!/bin/sh\nexit 99\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let engine = LatexmkEngine::new(LatexmkConfig::default().with_latexmk(&fake));
        let cancel = texrun_core::CancelToken::new();
        cancel.cancel();
        let ctx = CompileContext::new(&root).with_cancel(cancel);
        let run = engine.run(&ctx, &request("main.tex")).unwrap();
        assert_eq!(run.result.outcome, CompileOutcome::Cancelled);
        assert_eq!(run.pid, 0);
        assert!(run.result.exit.is_none());
    }

    /// End to end without TeX: a stand-in for latexmk records what it was
    /// given.
    #[test]
    fn fake_latexmk_sees_cleared_env_argv_and_cwd() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = workspace_with(&["src/main.tex", "src/chapters/intro.tex"]);
        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("latexmk");
        // Reads the start line (Linux), then records env, args and cwd.
        fs::write(
            &fake,
            "#!/bin/sh\nread -r line\nenv > \"$HOME/env.txt\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$HOME/args.txt\"\npwd > \"$HOME/cwd.txt\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let engine = LatexmkEngine::new(LatexmkConfig::default().with_search_path(format!(
            "{}:relative/bin::/usr/bin:/bin",
            bin.path().display()
        )));
        let run = engine
            .run(&CompileContext::new(&root), &request("src/main.tex"))
            .unwrap();
        // Exit 0 but no PDF.
        assert_eq!(run.result.outcome, CompileOutcome::Failed);
        assert!(
            run.result
                .errors()
                .any(|d| d.message.contains("without producing a PDF"))
        );
        assert_eq!(run.result.exit.unwrap().code, Some(0));

        let home = root.path().join(".texrun/home");
        let env = fs::read_to_string(home.join("env.txt")).unwrap();
        let mut names: Vec<&str> = env
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            // Set by the shell itself.
            .filter(|k| !matches!(*k, "PWD" | "OLDPWD" | "SHLVL" | "_"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "HOME",
                "LC_ALL",
                "MKTEXFMT",
                "MKTEXMF",
                "MKTEXPK",
                "MKTEXTEX",
                "MKTEXTFM",
                "PATH",
                "max_print_line",
                "openin_any",
                "openout_any"
            ]
        );
        assert!(env.contains(&format!("PATH={}:/usr/bin:/bin\n", bin.path().display())));
        assert!(env.contains(&format!("HOME={}\n", home.display())));

        let args = fs::read_to_string(home.join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "-norc");
        assert_eq!(args[1], "-r");
        assert!(!Path::new(args[2]).starts_with(root.path()));
        assert!(!Path::new(args[2]).exists(), "rc directory is removed");
        assert_eq!(
            args[8],
            format!("-outdir={}", root.path().join(".texrun/out").display())
        );
        assert_eq!(args[9], "./main.tex");

        let cwd = fs::read_to_string(home.join("cwd.txt")).unwrap();
        assert_eq!(cwd.trim_end(), root.path().join("src").to_str().unwrap());
        // Source directories were mirrored into the output directory.
        assert!(root.path().join(".texrun/out/chapters").is_dir());
    }

    #[test]
    fn info_is_cheap_and_names_the_engine() {
        let engine = LatexmkEngine::default();
        let info = engine.info();
        assert_eq!(info.name, "texlive");
        assert_eq!(info.version, None);
    }
}
