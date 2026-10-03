//! The preview tools in a container of the engine image (#46,
//! docs/security.md §4 "preview").

use std::path::{Path, PathBuf};
use std::time::Duration;

use texrun_sandbox::{ContainerLimits, ContainerSpec, Mount, Runtime};

use crate::process::{CPU_KILL_GRACE, CPU_TIME_MARGIN, TOOL_CPUS, TOOL_MEMORY, TOOL_PROCESSES};
use crate::tools::{MUTOOL, PDFINFO, PDFTOPPM};

/// `PATH` of the tools in the container, and where they are in the image
/// (`docker/engine/Dockerfile`).
pub(crate) const GUEST_PATH: &str = "/usr/bin:/bin";
const GUEST_BIN: &str = "/usr/bin";

/// Where the scratch directories are mounted in the container: below
/// `/texrun`, which the image leaves to texrun.
pub(crate) const GUEST_INPUT: &str = "/texrun/preview/in";
pub(crate) const GUEST_WORK: &str = "/texrun/preview/work";
pub(crate) const GUEST_HOME: &str = "/texrun/preview/home";

/// Name of the copy of the PDF in [`GUEST_INPUT`].
pub(crate) const PDF_NAME: &str = "document.pdf";

/// Added to the preview timeout for the lifetime of the container: texrun
/// removes it at the end of the preview; the lifetime only matters if
/// texrun is killed.
const LIFETIME_MARGIN: Duration = Duration::from_secs(30);

/// Runs the preview tools in a container of the engine image instead of on
/// the host ([`Previewer::in_container`](crate::Previewer::in_container)).
///
/// One container serves a whole preview run
/// ([`texrun_sandbox::Session`]): the tools run in it one after another.
/// It has the restrictions of every texrun container (no network, a
/// read-only root filesystem, no capabilities, a non-root user, see
/// `texrun-sandbox`), with the preview limits of docs/security.md §3.10 as
/// its cgroup limits (memory 2 GiB, 32 processes and threads, 2 CPUs) and
/// the rlimits of each tool set by `prlimit` before it starts. It sees
/// only a copy of the PDF (read-only) and the tools' private working
/// directory and `HOME` (writable), all in a scratch directory that texrun
/// creates outside the output root.
#[derive(Debug, Clone)]
pub struct PreviewContainer {
    runtime: Runtime,
    image: String,
    oci_runtime: Option<String>,
    scratch_parent: Option<PathBuf>,
}

impl PreviewContainer {
    /// Containers of `image` (which must exist locally and contain the
    /// tools, like the engine image) on `runtime`.
    pub fn new(runtime: Runtime, image: impl Into<String>) -> Self {
        Self {
            runtime,
            image: image.into(),
            oci_runtime: None,
            scratch_parent: None,
        }
    }

    /// `--runtime` of the container (e.g. `runsc`); `None`: the daemon's
    /// default.
    #[must_use]
    pub fn with_oci_runtime(mut self, runtime: Option<String>) -> Self {
        self.oci_runtime = runtime;
        self
    }

    /// Directory in which the scratch directory of each preview run is
    /// created. `None`: the system temporary directory. It must be visible
    /// to the runtime (on macOS: shared with its VM), and either not
    /// writable by others or sticky (like `/tmp`): the scratch directory is
    /// created in it with a random name and mode 0700, and it must not be
    /// possible for others to rename or remove it while the runtime mounts
    /// it by path.
    #[must_use]
    pub fn with_scratch_parent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.scratch_parent = Some(dir.into());
        self
    }

    /// The runtime.
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The image.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Where the scratch directory is created (not canonical).
    pub(crate) fn scratch_parent(&self) -> PathBuf {
        self.scratch_parent
            .clone()
            .unwrap_or_else(std::env::temp_dir)
    }

    /// The container for a preview with `timeout`, seeing `input`
    /// (read-only), `work` and `home` (writable): canonical host paths.
    pub(crate) fn spec(&self, input: &Path, work: &Path, home: &Path) -> ContainerSpec {
        ContainerSpec::new(
            self.image.clone(),
            ContainerLimits::new(TOOL_MEMORY, TOOL_PROCESSES, TOOL_CPUS),
        )
        .with_oci_runtime(self.oci_runtime.clone())
        .with_mount(Mount::read_only(input, GUEST_INPUT))
        .with_mount(Mount::writable(work, GUEST_WORK))
        .with_mount(Mount::writable(home, GUEST_HOME))
    }
}

/// How long the container of a preview with `timeout` may stay up.
pub(crate) fn lifetime(timeout: Duration) -> Duration {
    timeout
        .saturating_add(CPU_TIME_MARGIN)
        .saturating_add(Duration::from_secs(CPU_KILL_GRACE))
        .saturating_add(LIFETIME_MARGIN)
        .min(Duration::from_secs(u64::from(u32::MAX)))
}

/// The tools of the image, as absolute paths in the container.
pub(crate) fn guest_tools() -> [PathBuf; 3] {
    [MUTOOL, PDFINFO, PDFTOPPM].map(|name| Path::new(GUEST_BIN).join(name))
}

/// Which of `tools` the output of `ls -1 -- <tools>` lists (it prints the
/// existing ones and fails on the others).
pub(crate) fn listed(stdout: &str, tools: &[PathBuf]) -> Vec<PathBuf> {
    tools
        .iter()
        .filter(|tool| stdout.lines().any(|line| Path::new(line.trim()) == *tool))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lifetime_follows_the_timeout() {
        assert_eq!(
            lifetime(Duration::from_secs(30)),
            Duration::from_secs(30 + 10 + 5 + 30)
        );
        assert_eq!(
            lifetime(Duration::MAX),
            Duration::from_secs(u64::from(u32::MAX))
        );
    }

    #[test]
    fn listed_tools_are_matched_exactly() {
        let tools = guest_tools();
        assert_eq!(
            tools,
            ["/usr/bin/mutool", "/usr/bin/pdfinfo", "/usr/bin/pdftoppm"].map(PathBuf::from)
        );
        assert_eq!(
            listed("/usr/bin/pdfinfo\n/usr/bin/pdftoppm\n", &tools),
            ["/usr/bin/pdfinfo", "/usr/bin/pdftoppm"].map(PathBuf::from)
        );
        assert!(listed("/usr/bin/mutoolx\nmutool\n", &tools).is_empty());
    }

    #[test]
    fn the_mounts_are_below_texruns_own_directory() {
        for guest in [GUEST_INPUT, GUEST_WORK, GUEST_HOME] {
            assert!(Path::new(guest).starts_with("/texrun/preview"), "{guest}");
        }
    }
}
