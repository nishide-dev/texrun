//! Previews rendered in a container of the engine image (#46,
//! docs/security.md §4 "preview"): what is specific to the container. The
//! rendering itself is covered by `real_tools.rs` with
//! `TEXRUN_TEST_BACKEND=container`.
//!
//! These tests need a container runtime and the engine image
//! (`TEXRUN_SANDBOX_IMAGE`, or `texrun-engine:latest`). Without them they
//! are skipped, unless `TEXRUN_REQUIRE_SANDBOX=1` (CI's `sandbox` job).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use texrun_preview::{NoticeKind, PreviewContainer, PreviewOptions, PreviewStatus, Previewer};
use texrun_sandbox::{DEFAULT_IMAGE, LABEL, Runtime};

const REQUIRE_ENV: &str = "TEXRUN_REQUIRE_SANDBOX";
const IMAGE_ENV: &str = "TEXRUN_SANDBOX_IMAGE";

/// The runtime and the ID of the image, if they can be used.
fn sandbox() -> Option<&'static (Runtime, String)> {
    static SANDBOX: OnceLock<Result<(Runtime, String), String>> = OnceLock::new();
    let sandbox = SANDBOX.get_or_init(|| {
        let image = std::env::var(IMAGE_ENV)
            .ok()
            .filter(|i| !i.is_empty())
            .unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
        let runtime = Runtime::detect(None).map_err(|e| e.to_string())?;
        let id = runtime.image_id(&image).map_err(|e| e.to_string())?;
        Ok((runtime, id))
    });
    match sandbox {
        Ok(sandbox) => Some(sandbox),
        Err(e) if std::env::var_os(REQUIRE_ENV).is_some_and(|v| v == "1") => {
            panic!("the container sandbox is required ({REQUIRE_ENV}=1) but not usable: {e}")
        }
        Err(e) => {
            static REPORTED: OnceLock<()> = OnceLock::new();
            REPORTED.get_or_init(|| {
                let _ = writeln!(
                    std::io::stderr(),
                    "texrun-preview container: SKIPPED ({e}; set {REQUIRE_ENV}=1 to fail instead)"
                );
            });
            None
        }
    }
}

/// The tests look for the containers of this process, so they run one at
/// a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

macro_rules! require_sandbox {
    () => {
        match sandbox() {
            Some(sandbox) => (
                sandbox,
                SERIAL
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
            None => return,
        }
    };
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Containers of this test process (any state).
fn our_containers(runtime: &Runtime) -> Vec<String> {
    let out = Command::new(runtime.program())
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg(format!("label={LABEL}.pid={}", std::process::id()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn previews_are_rendered_in_a_container_that_is_removed_afterwards() {
    let ((runtime, image), _serial) = require_sandbox!();
    let scratch = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let previewer = Previewer::in_container(
        PreviewContainer::new(runtime.clone(), image.clone()).with_scratch_parent(scratch.path()),
    );
    // No host tool is looked up.
    assert!(previewer.toolset().available().is_empty());
    let report = previewer
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &PreviewOptions::default(),
        )
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Rendered, "{report:?}");
    assert_eq!(report.pages.len(), 3);
    for page in &report.pages {
        let bytes = fs::read(out.path().join(page.artifact.path.as_path())).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
    // Only the images reach the output root; the scratch directory (with
    // the copy of the PDF) and the container are gone.
    let names: Vec<_> = fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec!["preview"]);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    assert!(our_containers(runtime).is_empty());
}

#[test]
fn a_symlink_in_the_output_root_is_not_followed() {
    let ((runtime, image), _serial) = require_sandbox!();
    let out = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), out.path().join("preview")).unwrap();
    let report = Previewer::in_container(PreviewContainer::new(runtime.clone(), image.clone()))
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &PreviewOptions::default(),
        )
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.kind == NoticeKind::OutputError),
        "{report:?}"
    );
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[test]
fn without_a_usable_container_no_tool_runs_at_all() {
    let ((runtime, _), _serial) = require_sandbox!();
    let out = tempfile::tempdir().unwrap();
    // Fail closed: no fallback to the host's tools.
    let report = Previewer::in_container(PreviewContainer::new(
        runtime.clone(),
        "texrun-no-such-image:0",
    ))
    .render(
        &fixture("sizes.pdf"),
        out.path(),
        &PreviewOptions::default(),
    )
    .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped, "{report:?}");
    assert!(report.pages.is_empty());
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.kind == NoticeKind::ToolUnavailable
                && n.message.contains("preview container")),
        "{report:?}"
    );
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    assert!(our_containers(runtime).is_empty());
}

#[test]
fn a_cancelled_preview_removes_its_container() {
    let ((runtime, image), _serial) = require_sandbox!();
    let out = tempfile::tempdir().unwrap();
    let options = PreviewOptions::default().with_pages("1-".parse().unwrap());
    let cancel = options.cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        cancel.cancel();
    });
    let report = Previewer::in_container(PreviewContainer::new(runtime.clone(), image.clone()))
        .render(&fixture("many-pages.pdf"), out.path(), &options)
        .unwrap();
    trigger.join().unwrap();
    // Cancelled at some point (or done before the cancellation).
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.kind == NoticeKind::Cancelled)
            || report.status == PreviewStatus::Rendered,
        "{report:?}"
    );
    assert!(our_containers(runtime).is_empty());
}
