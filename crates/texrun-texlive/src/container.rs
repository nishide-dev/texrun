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
    Container, ContainerLimits, ContainerSpec, Mount, RESTRICTIONS_NOT_APPLIED, Runtime,
    RuntimeKind, SandboxError,
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

/// Where the workspace may be mounted in the container
/// ([`CompileContext::path_mapping`]): [`DEFAULT_GUEST_ROOT`] or below it,
/// or below one of the other directories here. Anywhere else the untrusted
/// workspace could hide the image's own programs and configuration (`/usr`,
/// `/etc`, ...) or texrun's rc, or collide with the runtime's mounts.
pub const GUEST_ROOT_PARENTS: &[&str] = &["/srv", "/mnt"];

/// What a custom image (`--container-image`) must provide, for the error of
/// a probe that failed in it (docs/security.md §4 "engine image").
const IMAGE_REQUIREMENTS: &str = "the image must provide /usr/bin/latexmk with pdflatex, bibtex \
     and makeindex in /usr/bin, /usr/bin/timeout, /usr/bin/prlimit and /bin/sh; see \
     docs/security.md §4";

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
    /// The engine image; must exist locally (texrun never pulls). Default:
    /// [`texrun_sandbox::DEFAULT_IMAGE`], the image published for this
    /// version of texrun.
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
    /// ID of the image, so that compiles use the image the probe reported
    /// even if its tag moves.
    image_id: Mutex<Option<String>>,
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
            image_id: Mutex::new(None),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &ContainerConfig {
        &self.config
    }

    /// The runtime, detected now if it was not yet.
    ///
    /// Right after detecting it, the containers that killed texrun
    /// processes of this user left behind are removed
    /// ([`Runtime::reclaim_left_containers`], best effort): those of the
    /// compile and of the page previews, once they have stopped.
    pub fn runtime(&self) -> Result<Runtime, EngineError> {
        let mut runtime = self.runtime.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(runtime) = runtime.as_ref() {
            return Ok(runtime.clone());
        }
        let detected = Runtime::detect(self.config.runtime).map_err(sandbox_error)?;
        // A runtime that cannot list its containers fails the probe anyway.
        let _ = detected.reclaim_left_containers();
        *runtime = Some(detected.clone());
        Ok(detected)
    }

    /// The ID of the configured image: the one the last probe saw, or
    /// looked up now. The page previews (`texrun_preview::PreviewContainer`)
    /// use the same image.
    pub fn image_id(&self) -> Result<String, EngineError> {
        let runtime = self.runtime()?;
        self.resolve_image_id(&runtime)
    }

    fn resolve_image_id(&self, runtime: &Runtime) -> Result<String, EngineError> {
        let mut cached = self.image_id.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let id = runtime
            .image_id(&self.config.image)
            .map_err(sandbox_error)?;
        *cached = Some(id.clone());
        Ok(id)
    }

    /// Compiles like [`TypesetEngine::compile`] and also returns the
    /// captured console output (as [`LatexmkEngine::run`]).
    pub fn run(
        &self,
        ctx: &CompileContext<'_>,
        request: &CompileRequest,
    ) -> Result<LatexmkRun, EngineError> {
        request.validate()?;
        let mapping = match &ctx.path_mapping {
            Some(mapping) => mapping.clone(),
            None => PathMapping::new(DEFAULT_GUEST_ROOT).expect("valid constant"),
        };
        check_guest_root(mapping.guest_root())?;
        let runtime = self.runtime()?;
        let image = self.resolve_image_id(&runtime)?;
        let sandbox = SandboxRun {
            runtime: &runtime,
            config: &self.config,
            image,
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
    /// compile, and nothing mounted). A process in that container must have
    /// no capability ([`texrun_sandbox::capability_report`]); otherwise the
    /// engine is unavailable.
    fn probe(&self) -> Result<EngineInfo, EngineError> {
        let runtime = self.runtime()?;
        let image = runtime.image(&self.config.image).map_err(sandbox_error)?;
        let image_id = image.id;
        *self.image_id.lock().unwrap_or_else(PoisonError::into_inner) = Some(image_id.clone());
        let spec = container_spec(&self.config, &image_id, PROBE_TIMEOUT);
        let container = Container::new(&runtime, spec);
        // latexmk under a shell that first reports the capability sets of
        // a process in the container: the `HostConfig` check reads what
        // the runtime recorded, this what the process has.
        let (program, args) = texrun_sandbox::capability_report(
            Some(Path::new(GUEST_LATEXMK)),
            &["-norc".into(), "-v".into()],
        );
        let job = Job {
            program: &program,
            args,
            cwd: Path::new("/tmp"),
            env: command::child_env(OsStr::new(GUEST_PATH), Path::new("/tmp"), None),
            timeout: Some(PROBE_TIMEOUT),
            cancel: None,
            size_dirs: Vec::new(),
            limits: self.config.limits,
            start: Start::Container(&container),
            cgroups: None,
        };
        let (mut finished, _) = finish(&container, process::run(&job))?;
        drop(container);
        let capabilities = texrun_sandbox::take_capability_report(&mut finished.stdout);
        let stdout = String::from_utf8_lossy(&finished.stdout.bytes);
        let version = match (finished.stop, engine::parse_version(&stdout)) {
            (None, Some(v)) if finished.status.success() => {
                // Fail closed, like a restriction missing from `HostConfig`.
                capabilities.map_err(|reason| {
                    unavailable(format!(
                        "{} {RESTRICTIONS_NOT_APPLIED}: {reason}",
                        runtime.kind()
                    ))
                })?;
                v
            }
            _ => {
                let stderr = String::from_utf8_lossy(&finished.stderr.bytes);
                return Err(unavailable(format!(
                    "`latexmk -v` in the image `{}` did not report a version (exit: {:?}): {} \
                     ({IMAGE_REQUIREMENTS})",
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
            "latexmk {version} ({} {}, image {} {short_id}, {})",
            runtime.kind(),
            runtime.version(),
            self.config.image,
            image_version(image.version.as_deref())
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

/// How [`TypesetEngine::probe`] reports the image's version label
/// ([`texrun_sandbox::IMAGE_VERSION_LABEL`]) in `engine.version`, so that a
/// result shows whether the image was published for this version of texrun.
fn image_version(label: Option<&str>) -> String {
    match label {
        Some(v) if v == texrun_sandbox::VERSION => format!("image version {v}"),
        Some(v) => format!(
            "image version {v}, not {} of texrun",
            texrun_sandbox::VERSION
        ),
        None => "image without a version label".to_owned(),
    }
}

/// What the engine needs to run one compile in a container.
pub(crate) struct SandboxRun<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) config: &'a ContainerConfig,
    /// The image, by ID.
    pub(crate) image: String,
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
        let spec = container_spec(self.config, &self.image, timeout)
            .with_mount(Mount::read_only(root, &guest.root))
            .with_mount(Mount::writable(output_dir, &guest.output_dir))
            .with_mount(Mount::writable(home, &guest.home))
            .with_mount(Mount::read_only(rc_dir, GUEST_RC_DIR));
        Container::new(self.runtime, spec)
    }
}

/// How a supervised run in `container` ended: a container the runtime did
/// not restrict as asked is [`EngineError::Unavailable`] (fail closed);
/// the runtime's warnings while creating it are returned as notes.
pub(crate) fn finish(
    container: &Container<'_>,
    result: Result<process::Finished, texrun_process::RunError>,
) -> Result<(process::Finished, Vec<String>), EngineError> {
    if let Some(reason) = container.refusal() {
        return Err(unavailable(reason));
    }
    let finished = result.map_err(process::engine_error)?;
    let notes = container
        .warnings()
        .into_iter()
        .map(|w| format!("container: {w}"))
        .collect();
    Ok((finished, notes))
}

/// Refuses a guest mount point outside [`DEFAULT_GUEST_ROOT`] and
/// [`GUEST_ROOT_PARENTS`].
pub(crate) fn check_guest_root(guest: &Path) -> Result<(), EngineError> {
    let below = |parent: &str| guest.starts_with(parent) && guest != Path::new(parent);
    let allowed =
        guest.starts_with(DEFAULT_GUEST_ROOT) || GUEST_ROOT_PARENTS.iter().any(|p| below(p));
    if allowed {
        Ok(())
    } else {
        Err(EngineError::InvalidRequest(format!(
            "the workspace cannot be mounted at {} in the container: use {DEFAULT_GUEST_ROOT} \
             (or a directory below it) or a directory below {}",
            guest.display(),
            GUEST_ROOT_PARENTS.join(" or ")
        )))
    }
}

/// The container settings of `config` for a run of `image` with `timeout`
/// (no mounts).
fn container_spec(config: &ContainerConfig, image: &str, timeout: Duration) -> ContainerSpec {
    let limits = &config.limits;
    let cpu_budget = Limits::CPU_TIME_MARGIN + Limits::CPU_KILL_GRACE + DEADLINE_MARGIN;
    // A timeout too large for `timeout` in the container: no deadline
    // there (texrun's own timeout still applies).
    let deadline = timeout
        .checked_add(cpu_budget)
        .filter(|d| u32::try_from(d.as_secs()).is_ok());
    ContainerSpec::new(
        image,
        ContainerLimits::new(
            limits.max_memory_bytes,
            limits.max_processes,
            limits.max_cpus,
        ),
    )
    .with_oci_runtime(config.oci_runtime.clone())
    .with_deadline(deadline)
    // `pids.events`: a process limit that refused latexmk's `fork` is a
    // `resource_limit` diagnostic (perl retries it until the timeout).
    .with_report_pids(true)
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
    fn the_image_version_is_compared_with_texruns() {
        let this = texrun_sandbox::VERSION;
        assert_eq!(image_version(Some(this)), format!("image version {this}"));
        assert_eq!(
            image_version(Some("0.0.0-other")),
            format!("image version 0.0.0-other, not {this} of texrun")
        );
        assert_eq!(image_version(None), "image without a version label");
        assert_eq!(
            ContainerConfig::default().image,
            format!("ghcr.io/nishide-dev/texrun-engine:{this}")
        );
    }

    #[test]
    fn the_container_deadline_follows_the_timeout() {
        let config = ContainerConfig::default();
        let spec = container_spec(&config, "sha256:0123", Duration::from_secs(60));
        assert_eq!(spec.deadline, Some(Duration::from_secs(60 + 10 + 5 + 30)));
        assert_eq!(spec.image, "sha256:0123");
        assert_eq!(spec.limits, ContainerLimits::new(4 << 30, 64, 2));
        assert_eq!(container_spec(&config, "x", Duration::MAX).deadline, None);
        assert!(spec.mounts.is_empty());
        assert!(spec.report_pids);
    }

    #[test]
    fn the_workspace_is_mounted_only_where_it_hides_nothing() {
        for ok in [
            "/workspace",
            "/workspace/project",
            "/srv/texrun ws/project",
            "/mnt/ws",
        ] {
            assert!(check_guest_root(Path::new(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "/usr",
            "/usr/bin",
            "/usr/share/texlive",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/etc",
            "/proc",
            "/sys",
            "/dev",
            "/tmp",
            "/run",
            "/var",
            "/home/u",
            "/texrun",
            "/texrun/rc/x",
            "/srv",
            "/mnt",
            "/workspace2",
        ] {
            assert!(
                matches!(
                    check_guest_root(Path::new(bad)),
                    Err(EngineError::InvalidRequest(_))
                ),
                "{bad}"
            );
        }
        assert!(!Path::new(GUEST_RC_DIR).starts_with(DEFAULT_GUEST_ROOT));
    }

    #[test]
    fn the_rc_is_outside_the_workspace_in_the_container() {
        assert_eq!(guest_rc_path(), Path::new("/texrun/rc/texrun.latexmkrc"));
        assert!(!Path::new(GUEST_RC_DIR).starts_with(DEFAULT_GUEST_ROOT));
    }
}
