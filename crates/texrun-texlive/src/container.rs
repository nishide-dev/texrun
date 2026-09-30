//! The container backend: latexmk in a hardened container (#26,
//! docs/security.md §4).

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use texrun_core::{
    CompileContext, CompileRequest, CompileResult, EngineError, EngineInfo, PathMapping,
    ProcessExit, TypesetEngine,
};
use texrun_sandbox::{
    Container, ContainerLimits, ContainerSpec, Mount, Runtime, RuntimeKind, SandboxError,
};

use crate::command;
use crate::engine::{self, GuestPaths, LatexmkConfig, LatexmkEngine, LatexmkRun, RC_FILE_NAME};
use crate::process::{self, Job, Limits, Start};

/// Engine identifier of [`ContainerEngine`] in [`EngineInfo::name`].
pub const CONTAINER_ENGINE_NAME: &str = "texlive-container";

/// Where the workspace is mounted in the container unless the context has
/// a [`CompileContext::path_mapping`].
pub const DEFAULT_GUEST_ROOT: &str = "/workspace";

/// latexmk in the image (docker/engine/Dockerfile).
pub const GUEST_LATEXMK: &str = "/usr/bin/latexmk";

/// `PATH` in the container: where the image has pdflatex, bibtex and
/// makeindex.
pub const GUEST_PATH: &str = "/usr/bin:/bin";

/// Where the rc directory is mounted in the container.
const GUEST_RC_DIR: &str = "/texrun/rc";

/// Timeout of `latexmk -v` in a container: includes starting the
/// container (and, on macOS, possibly the runtime's VM).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Added to the compile timeout for the container's own deadline
/// ([`ContainerSpec::deadline`]): texrun's timeout normally stops the run
/// first; the deadline only matters if texrun is killed.
const DEADLINE_MARGIN: Duration = Duration::from_secs(30);

/// Configuration of a [`ContainerEngine`].
///
/// `#[non_exhaustive]`: construct with [`ContainerConfig::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerConfig {
    /// Which runtime to use. `None`: Docker, else Podman (the first that is
    /// installed and reachable).
    pub runtime: Option<RuntimeKind>,
    /// The engine image; must exist locally (texrun never pulls).
    pub image: String,
    /// `--runtime` of the container (e.g. `runsc` for gVisor, if
    /// configured in the daemon). `None`: the daemon's default. Not tested
    /// by texrun.
    pub oci_runtime: Option<String>,
    /// Directory in which the per-compile rc directory is created. `None`:
    /// the system temporary directory. Must be outside the workspace, and
    /// visible to the runtime (on macOS: shared with its VM).
    pub rc_parent: Option<PathBuf>,
    /// Timeout used when the request has none.
    pub default_timeout: Duration,
    /// Size and resource limits. In the container, the CPU time, file size
    /// and address space limits are rlimits set by the runtime, and memory,
    /// processes and CPUs are the limits of the container's cgroup.
    pub limits: Limits,
    /// Sets `SOURCE_DATE_EPOCH` (and `FORCE_SOURCE_DATE=1`).
    pub source_date_epoch: Option<i64>,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        let engine = LatexmkConfig::default();
        Self {
            runtime: None,
            image: texrun_sandbox::DEFAULT_IMAGE.to_owned(),
            oci_runtime: None,
            rc_parent: None,
            default_timeout: engine.default_timeout,
            limits: engine.limits,
            source_date_epoch: None,
        }
    }
}

impl ContainerConfig {
    /// Sets [`ContainerConfig::runtime`].
    #[must_use]
    pub fn with_runtime(mut self, runtime: Option<RuntimeKind>) -> Self {
        self.runtime = runtime;
        self
    }

    /// Sets [`ContainerConfig::image`].
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Sets [`ContainerConfig::oci_runtime`].
    #[must_use]
    pub fn with_oci_runtime(mut self, runtime: Option<String>) -> Self {
        self.oci_runtime = runtime;
        self
    }

    /// Sets [`ContainerConfig::rc_parent`].
    #[must_use]
    pub fn with_rc_parent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.rc_parent = Some(dir.into());
        self
    }

    /// Sets [`ContainerConfig::default_timeout`].
    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets [`ContainerConfig::limits`].
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets [`ContainerConfig::source_date_epoch`].
    #[must_use]
    pub fn with_source_date_epoch(mut self, epoch: Option<i64>) -> Self {
        self.source_date_epoch = epoch;
        self
    }
}

/// TeX Live + latexmk in a hardened container (docs/security.md §4).
///
/// The same compile as [`LatexmkEngine`] — the texrun rc, latexmk's
/// arguments, the environment allowlist, kpathsea's paranoid mode, the
/// timeout and output limits, the log and BibTeX diagnostics — but latexmk
/// runs in a container of the [`ContainerConfig::image`] that sees only the
/// workspace (read-only, except the output directory and `HOME`) and the
/// rc (read-only), with no network, no capabilities, a read-only root
/// filesystem, a non-root user and the [`Limits`] as runtime limits (see
/// `texrun-sandbox`).
///
/// The workspace is mounted at [`CompileContext::path_mapping`], or
/// [`DEFAULT_GUEST_ROOT`]. Requests and results are the same as with the
/// host engine; [`EngineInfo::name`] is [`CONTAINER_ENGINE_NAME`].
///
/// A missing runtime or image is [`EngineError::Unavailable`] from
/// [`TypesetEngine::probe`] and from the compile.
#[derive(Debug)]
pub struct ContainerEngine {
    config: ContainerConfig,
    /// Runs the compile; its configuration mirrors `config`.
    inner: LatexmkEngine,
    /// The runtime, once detected.
    runtime: Mutex<Option<Runtime>>,
    /// Version string from the last successful probe.
    version: Mutex<Option<String>>,
}

impl Default for ContainerEngine {
    fn default() -> Self {
        Self::new(ContainerConfig::default())
    }
}

impl ContainerEngine {
    /// Creates an engine. Does not start anything; the runtime is detected
    /// on the first probe or compile.
    pub fn new(config: ContainerConfig) -> Self {
        let mut engine = LatexmkConfig::default()
            .with_default_timeout(config.default_timeout)
            .with_limits(config.limits)
            .with_source_date_epoch(config.source_date_epoch);
        if let Some(dir) = &config.rc_parent {
            engine = engine.with_rc_parent(dir);
        }
        Self {
            config,
            inner: LatexmkEngine::new(engine),
            runtime: Mutex::new(None),
            version: Mutex::new(None),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &ContainerConfig {
        &self.config
    }

    /// The runtime, detected now if it was not yet.
    pub fn runtime(&self) -> Result<Runtime, EngineError> {
        let mut runtime = self.runtime.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(runtime) = runtime.as_ref() {
            return Ok(runtime.clone());
        }
        let detected = Runtime::detect(self.config.runtime).map_err(sandbox_error)?;
        *runtime = Some(detected.clone());
        Ok(detected)
    }

    /// Compiles like [`TypesetEngine::compile`] and also returns the
    /// captured console output (as [`LatexmkEngine::run`]).
    pub fn run(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<LatexmkRun, EngineError> {
        request.validate()?;
        let runtime = self.runtime()?;
        let mapping = match &ctx.path_mapping {
            Some(mapping) => mapping.clone(),
            None => PathMapping::new(DEFAULT_GUEST_ROOT).expect("valid constant"),
        };
        if mapping.guest_root().starts_with(GUEST_RC_DIR)
            || Path::new(GUEST_RC_DIR).starts_with(mapping.guest_root())
        {
            return Err(EngineError::InvalidRequest(format!(
                "the workspace cannot be mounted at {} in the container: {GUEST_RC_DIR} is \
                 texrun's",
                mapping.guest_root().display()
            )));
        }
        let sandbox = SandboxRun {
            runtime: &runtime,
            config: &self.config,
            mapping,
            latexmk: Path::new(GUEST_LATEXMK),
            search_path: OsStr::new(GUEST_PATH),
        };
        let mut run = self.inner.run_in(ctx, request, Some(&sandbox))?;
        run.result.engine = self.info();
        Ok(run)
    }
}

impl TypesetEngine for ContainerEngine {
    fn info(&self) -> EngineInfo {
        let info = EngineInfo::new(CONTAINER_ENGINE_NAME);
        match self
            .version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            Some(v) => info.with_version(v.clone()),
            None => info,
        }
    }

    /// Detects the runtime, checks that the image exists and runs
    /// `latexmk -v` in a container of it (with the same restrictions as a
    /// compile, and nothing mounted).
    fn probe(&self) -> Result<EngineInfo, EngineError> {
        let runtime = self.runtime()?;
        let image_id = runtime
            .image_id(&self.config.image)
            .map_err(sandbox_error)?;
        let spec = container_spec(&self.config, PROBE_TIMEOUT);
        let container = Container::new(&runtime, spec);
        let job = Job {
            program: Path::new(GUEST_LATEXMK),
            args: vec!["-norc".into(), "-v".into()],
            cwd: Path::new("/tmp"),
            env: command::child_env(OsStr::new(GUEST_PATH), Path::new("/tmp"), None),
            timeout: Some(PROBE_TIMEOUT),
            cancel: None,
            size_dirs: Vec::new(),
            limits: self.config.limits,
            start: Start::Container(&container),
            cgroups: None,
        };
        let finished = process::run(&job).map_err(process::engine_error)?;
        drop(container);
        let stdout = String::from_utf8_lossy(&finished.stdout.bytes);
        let version = match (finished.stop, engine::parse_version(&stdout)) {
            (None, Some(v)) if finished.status.success() => v,
            _ => {
                let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
                return Err(unavailable(format!(
                    "`latexmk -v` in the image `{}` did not report a version (exit: {:?}): {}",
                    self.config.image,
                    ProcessExit::from(finished.status),
                    stderr.trim()
                )));
            }
        };
        let short_id = image_id
            .strip_prefix("sha256:")
            .unwrap_or(&image_id)
            .get(..12)
            .unwrap_or(&image_id)
            .to_owned();
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!(
            "latexmk {version} ({} {}, image {} {short_id})",
            runtime.kind(),
            runtime.version(),
            self.config.image
        ));
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

/// What the engine needs to run one compile in a container.
pub(crate) struct SandboxRun<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) config: &'a ContainerConfig,
    /// Where the workspace is in the container.
    pub(crate) mapping: PathMapping,
    /// latexmk in the container.
    pub(crate) latexmk: &'a Path,
    /// `PATH` in the container.
    pub(crate) search_path: &'a OsStr,
}

impl SandboxRun<'_> {
    /// The container for one compile: the workspace `root` read-only at
    /// the guest root, the (canonical) `output_dir` and `home` writable,
    /// the rc directory read-only.
    pub(crate) fn container(
        &self,
        root: &Path,
        output_dir: &Path,
        home: &Path,
        rc_dir: &Path,
        guest: &GuestPaths,
        timeout: Duration,
    ) -> Container<'_> {
        let spec = container_spec(self.config, timeout)
            .with_mount(Mount::read_only(root, &guest.root))
            .with_mount(Mount::writable(output_dir, &guest.output_dir))
            .with_mount(Mount::writable(home, &guest.home))
            .with_mount(Mount::read_only(rc_dir, GUEST_RC_DIR));
        Container::new(self.runtime, spec)
    }
}

/// The container settings of `config` for a run with `timeout` (no
/// mounts).
fn container_spec(config: &ContainerConfig, timeout: Duration) -> ContainerSpec {
    let limits = &config.limits;
    let cpu_budget = Limits::CPU_TIME_MARGIN + Limits::CPU_KILL_GRACE + DEADLINE_MARGIN;
    // A timeout too large for `timeout` in the container: no deadline
    // there (texrun's own timeout still applies).
    let deadline = timeout
        .checked_add(cpu_budget)
        .filter(|d| u32::try_from(d.as_secs()).is_ok());
    ContainerSpec::new(
        config.image.clone(),
        ContainerLimits::new(
            limits.max_memory_bytes,
            limits.max_processes,
            limits.max_cpus,
        ),
    )
    .with_oci_runtime(config.oci_runtime.clone())
    .with_deadline(deadline)
}

/// The rc as latexmk sees it in the container.
pub(crate) fn guest_rc_path() -> PathBuf {
    Path::new(GUEST_RC_DIR).join(RC_FILE_NAME)
}

/// Makes the rc directory and file readable by the container user (who
/// may be a different uid, e.g. when texrun runs as root). The rc holds no
/// secret; it is mounted read-only.
pub(crate) fn share_rc(dir: &Path, rc: &Path) -> Result<(), EngineError> {
    for (path, mode) in [(dir, 0o755), (rc, 0o644)] {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
            EngineError::Io {
                context: format!("making {} readable for the container", path.display()),
                source,
            }
        })?;
    }
    Ok(())
}

fn unavailable(reason: String) -> EngineError {
    EngineError::Unavailable {
        engine: CONTAINER_ENGINE_NAME.to_owned(),
        reason,
    }
}

/// The engine error for a sandbox that cannot be used.
fn sandbox_error(e: SandboxError) -> EngineError {
    match e {
        SandboxError::Invalid(reason) => EngineError::InvalidRequest(reason),
        other => unavailable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_deadline_follows_the_timeout() {
        let config = ContainerConfig::default();
        let spec = container_spec(&config, Duration::from_secs(60));
        assert_eq!(spec.deadline, Some(Duration::from_secs(60 + 10 + 5 + 30)));
        assert_eq!(spec.image, texrun_sandbox::DEFAULT_IMAGE);
        assert_eq!(spec.limits, ContainerLimits::new(4 << 30, 64, 2));
        assert_eq!(container_spec(&config, Duration::MAX).deadline, None);
        assert!(spec.mounts.is_empty());
    }

    #[test]
    fn the_rc_is_outside_the_workspace_in_the_container() {
        assert_eq!(guest_rc_path(), Path::new("/texrun/rc/texrun.latexmkrc"));
        assert!(!Path::new(GUEST_RC_DIR).starts_with(DEFAULT_GUEST_ROOT));
    }
}
