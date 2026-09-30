//! Degraded behavior and process limits, tested with stand-in tools.
//!
//! These tests do not need Poppler or `MuPDF`: small `sh` scripts named
//! `pdfinfo` / `pdftoppm` play the tools, so they run on every CI runner.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use texrun_core::{CancelToken, Severity};
use texrun_preview::{
    BackendChoice, ExecGate, NoticeKind, PreviewError, PreviewOptions, PreviewReport,
    PreviewStatus, Previewer, Toolset,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// `pdfinfo` output for a 3-page document.
const PDFINFO: &str = r#"#!/bin/sh
echo "Title:           x"
echo "Pages:           3"
for p in 1 2 3; do
  echo "Page    $p size:  300 x 400 pts"
  echo "Page    $p rot:   0"
done
"#;

/// A `pdftoppm` that writes a tiny PNG header (2 x 3 px) to `<prefix>.png`
/// and fails on page 2. It also records its environment.
const PDFTOPPM: &str = r#"#!/bin/sh
for a; do last=$a; done
case " $* " in
  *" -f 2 "*) echo "Syntax Error: broken page" >&2; exit 1 ;;
esac
env > "$HOME/../env.txt"
printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\000\002\000\000\000\003' > "$last.png"
"#;

struct Fake {
    bin: tempfile::TempDir,
    tools: Toolset,
}

/// A toolset whose `pdfinfo` / `pdftoppm` are the given scripts. The search
/// path also contains the system directories so that the scripts can use
/// standard commands.
fn fake(pdfinfo: &str, pdftoppm: &str) -> Fake {
    let bin = tempfile::tempdir().unwrap();
    for (name, script) in [("pdfinfo", pdfinfo), ("pdftoppm", pdftoppm)] {
        let path = bin.path().join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut search = OsString::from(bin.path());
    search.push(":/usr/bin:/bin");
    let tools = Toolset::from_search_path(Some(&search));
    Fake { bin, tools }
}

/// Options that select the (fake) Poppler tools, even where a real `mutool`
/// is installed in the system directories.
fn opts() -> PreviewOptions {
    PreviewOptions::default().with_backend(BackendChoice::Poppler)
}

fn kinds(report: &PreviewReport) -> Vec<NoticeKind> {
    report.notices.iter().map(|n| n.kind).collect()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn missing_tools_skip_previews_with_a_warning() {
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(Toolset::none())
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped);
    assert_eq!(report.backend, None);
    assert!(report.pages.is_empty() && report.pdf.is_none());
    assert_eq!(kinds(&report), vec![NoticeKind::ToolUnavailable]);
    assert_eq!(report.notices[0].severity, Severity::Warning);
    assert!(entries(out.path()).is_empty(), "nothing written");

    // An explicitly requested backend that is missing is reported the same
    // way. Only the fake directory is searched, so a real `mutool` in the
    // system directories is not found.
    let f = fake(PDFINFO, PDFTOPPM);
    let only_fakes = Toolset::from_search_path(Some(f.bin.path().as_os_str()));
    let report = Previewer::new(only_fakes)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &PreviewOptions::default().with_backend(BackendChoice::Mupdf),
        )
        .unwrap();
    assert_eq!(kinds(&report), vec![NoticeKind::ToolUnavailable]);
    assert!(report.notices[0].message.contains("mutool"));
}

#[test]
fn only_invalid_options_are_an_error() {
    let out = tempfile::tempdir().unwrap();
    let err = Previewer::new(Toolset::none())
        .render(&fixture("sizes.pdf"), out.path(), &opts().with_dpi(0))
        .unwrap_err();
    assert!(matches!(err, PreviewError::InvalidOptions(_)));
}

#[test]
fn a_failing_page_does_not_stop_the_others() {
    let f = fake(PDFINFO, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(f.tools)
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();

    assert_eq!(report.status, PreviewStatus::Partial);
    let pages: Vec<_> = report.pages.iter().map(|p| p.artifact.page).collect();
    assert_eq!(pages, vec![Some(1), Some(3)]);
    assert_eq!(
        report.pages[0].artifact.path.as_str(),
        "preview/page-001.png"
    );
    assert_eq!(
        (report.pages[0].width_px, report.pages[0].height_px),
        (2, 3)
    );
    assert_eq!(report.pages[0].artifact.size_bytes, Some(24));

    assert_eq!(kinds(&report), vec![NoticeKind::RenderFailed]);
    let notice = &report.notices[0];
    assert_eq!(notice.page, Some(2));
    assert_eq!(notice.detail.as_deref(), Some("Syntax Error: broken page"));

    // Only the images remain; the scratch directory is gone.
    assert_eq!(entries(out.path()), vec!["preview"]);
    assert_eq!(
        entries(&out.path().join("preview")),
        vec!["page-001.png", "page-003.png"]
    );
}

#[test]
fn tools_run_with_a_cleared_environment() {
    // The fake writes `env` next to its HOME, inside the scratch directory,
    // which is removed afterwards; keep a copy by pointing the scratch
    // directory's parent at a directory we control.
    let script = PDFTOPPM.replace(
        "env > \"$HOME/../env.txt\"",
        "env > \"$HOME/../../env.txt\"",
    );
    let f = fake(PDFINFO, &script);
    let out = tempfile::tempdir().unwrap();
    Previewer::new(f.tools)
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    let env = fs::read_to_string(out.path().join("env.txt")).unwrap();
    let names: Vec<_> = env
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    // PWD / SHLVL / _ / OLDPWD are set by `sh` itself.
    let allowed = ["PATH", "HOME", "LC_ALL", "PWD", "SHLVL", "_", "OLDPWD"];
    for name in &names {
        assert!(
            allowed.contains(name),
            "unexpected variable {name} in {env}"
        );
    }
    assert!(env.lines().any(|l| l == "LC_ALL=C"), "{env}");
    let home = env.lines().find_map(|l| l.strip_prefix("HOME=")).unwrap();
    assert!(home.contains(".texrun-preview-"), "{home}");
}

#[test]
fn unreadable_pdf_is_a_notice() {
    let failing = "#!/bin/sh\necho 'Syntax Error: Couldn'\"'\"'t read xref table' >&2\nexit 1\n";
    let f = fake(failing, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(f.tools.clone())
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped);
    assert_eq!(kinds(&report), vec![NoticeKind::PdfUnreadable]);
    assert!(
        report.notices[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("xref")
    );

    // A missing PDF is reported without running any tool.
    let report = Previewer::new(f.tools)
        .render(&out.path().join("missing.pdf"), out.path(), &opts())
        .unwrap();
    assert_eq!(kinds(&report), vec![NoticeKind::PdfUnreadable]);
}

#[test]
fn timeout_kills_the_whole_process_group() {
    // The tool starts a background job that keeps appending to a file, then
    // hangs. After the timeout, the file must stop growing: the background
    // job was killed along with the tool (docs/security.md §3.6).
    let dir = tempfile::tempdir().unwrap();
    let heart = dir.path().join("heartbeat");
    // The tool writes the first beat itself, before it starts the job, so
    // the file is never empty when the timeout hits, however slowly the
    // job gets going under load.
    let script = format!(
        "#!/bin/sh\nheart='{}'\necho x >> \"$heart\"\n\
         (while :; do echo x >> \"$heart\"; sleep 0.05; done) &\nsleep 30\n",
        heart.display()
    );
    let f = fake(&script, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_timeout(Duration::from_secs(5)),
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(kinds(&report), vec![NoticeKind::TimedOut]);
    assert_eq!(report.status, PreviewStatus::Skipped);

    let size = || fs::metadata(&heart).map_or(0, |m| m.len());
    let before = size();
    assert!(before > 0, "the background job never ran");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(size(), before, "a descendant survived the timeout");
}

#[test]
fn a_growing_image_is_stopped_at_the_size_limit() {
    let grow = "#!/bin/sh\nfor a; do last=$a; done\n\
                while :; do head -c 65536 /dev/zero >> \"$last.png\"; done\n";
    let f = fake(PDFINFO, grow);
    let out = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_max_total_bytes(200_000),
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(report.status, PreviewStatus::Skipped);
    assert_eq!(kinds(&report), vec![NoticeKind::SizeLimit]);
    assert_eq!(report.notices[0].page, Some(1));
    assert!(entries(&out.path().join("preview")).is_empty());
}

#[test]
fn cancellation_stops_the_run() {
    let f = fake(PDFINFO, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_cancel(cancel),
        )
        .unwrap();
    assert_eq!(kinds(&report), vec![NoticeKind::Cancelled]);
    assert_eq!(report.status, PreviewStatus::Skipped);
}

#[test]
fn cancelling_a_running_tool_stops_its_process_group() {
    // Like the timeout test, but the run is stopped by cancelling the token
    // from another thread while the tool (and its background job) runs.
    let dir = tempfile::tempdir().unwrap();
    let heart = dir.path().join("heartbeat");
    let script = format!(
        "#!/bin/sh\nheart='{}'\n(while :; do echo x >> \"$heart\"; sleep 0.05; done) &\nsleep 30\n",
        heart.display()
    );
    let f = fake(&script, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let heart = heart.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            while !heart.exists() && started.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            cancel.cancel();
        })
    };
    let started = Instant::now();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_cancel(cancel),
        )
        .unwrap();
    canceller.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(kinds(&report), vec![NoticeKind::Cancelled]);
    assert_eq!(report.status, PreviewStatus::Skipped);

    let size = || fs::metadata(&heart).map_or(0, |m| m.len());
    let before = size();
    assert!(before > 0, "the background job never ran");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(size(), before, "a descendant survived the cancellation");
}

#[cfg(target_os = "linux")]
#[test]
fn tools_run_with_resource_limits_on_linux() {
    // The fake records its own limits next to the output root, then renders
    // as usual. The limits are set with `prlimit` after the tool started
    // (`StartMode::Immediate`), so it waits (up to 5 s) until they have
    // arrived at itself, and reads its own entry rather than one inherited
    // by a child (a child started before the limits would not have them).
    let script = PDFTOPPM.replace(
        "env > \"$HOME/../env.txt\"",
        "i=0; while [ $i -lt 500 ]; do \
           grep -q '^Max address space *2147483648 ' /proc/$$/limits && break; \
           i=$((i+1)); sleep 0.01; \
         done; \
         cat /proc/$$/limits > \"$HOME/../../limits.txt\"",
    );
    let f = fake(PDFINFO, &script);
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_pages("1".parse().unwrap()),
        )
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Rendered);
    let limits = fs::read_to_string(out.path().join("limits.txt")).unwrap();
    let limit = |name: &str| {
        let line = limits
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing in {limits}"));
        line.split_whitespace().rev().nth(1).unwrap().to_owned()
    };
    // Columns: name, soft, hard, units; take the hard limit.
    assert_eq!(
        limit("Max address space"),
        (2u64 << 30).to_string(),
        "{limits}"
    );
    // The whole 128 MiB budget is left for page 1 (one byte more, so that
    // exceeding the budget is detectable).
    assert_eq!(
        limit("Max file size"),
        ((128u64 << 20) + 1).to_string(),
        "{limits}"
    );
    assert_eq!(limit("Max core file size"), "0", "{limits}");
}

#[test]
fn an_image_over_the_pixel_limit_is_discarded() {
    // The fake claims a 5000 x 10 px image, whatever the requested DPI: the
    // check after rendering must catch it.
    let big = "#!/bin/sh\nfor a; do last=$a; done\n\
               printf '\\211PNG\\r\\n\\032\\n\\000\\000\\000\\rIHDR\\000\\000\\023\\210\\000\\000\\000\\012' \
               > \"$last.png\"\n";
    let f = fake(PDFINFO, big);
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_pages("1".parse().unwrap()),
        )
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped);
    assert_eq!(kinds(&report), vec![NoticeKind::RenderFailed]);
    assert!(
        report.notices[0].message.contains("5000 x 10"),
        "{:?}",
        report.notices
    );
    assert!(entries(&out.path().join("preview")).is_empty());
}

#[test]
fn an_existing_symlink_at_the_image_name_is_replaced_not_followed() {
    let f = fake(PDFINFO, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let victim = elsewhere.path().join("victim");
    fs::write(&victim, "keep").unwrap();
    fs::create_dir(out.path().join("preview")).unwrap();
    std::os::unix::fs::symlink(&victim, out.path().join("preview/page-001.png")).unwrap();
    let report = Previewer::new(f.tools)
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &opts().with_pages("1".parse().unwrap()),
        )
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Rendered);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    let image = out.path().join("preview/page-001.png");
    assert!(fs::symlink_metadata(&image).unwrap().is_file());
}

#[test]
fn a_symlinked_preview_directory_is_refused() {
    let f = fake(PDFINFO, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), out.path().join("preview")).unwrap();
    let report = Previewer::new(f.tools)
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    assert_eq!(report.status, PreviewStatus::Skipped);
    assert_eq!(kinds(&report), vec![NoticeKind::OutputError]);
    assert!(entries(elsewhere.path()).is_empty());
}

#[test]
fn a_required_exec_gate_that_cannot_be_used_skips_the_previews() {
    // The fake records that it ran.
    let script = PDFINFO.replace("#!/bin/sh\n", "#!/bin/sh\ntouch \"$HOME/../../ran\"\n");
    let f = fake(&script, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    for gate in [
        ExecGate::new("/nonexistent/texrun"),
        ExecGate::unavailable("the path of the executable is unknown"),
    ] {
        let report = Previewer::new(f.tools.clone())
            .with_exec_gate(gate.with_required(true))
            .render(&fixture("sizes.pdf"), out.path(), &opts())
            .unwrap();
        assert_eq!(report.status, PreviewStatus::Skipped);
        assert_eq!(kinds(&report), vec![NoticeKind::ResourceLimits]);
        assert_eq!(report.notices[0].severity, Severity::Warning);
        assert!(report.pages.is_empty());
        assert!(!out.path().join("ran").exists(), "a tool ran");
    }
    let report = Previewer::new(f.tools)
        .with_exec_gate(ExecGate::unavailable("unknown").with_required(true))
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    // Visible in the JSON of the CLI as well.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["notices"][0]["kind"], "resource_limits", "{json}");
    assert!(
        json["notices"][0]["message"]
            .as_str()
            .unwrap()
            .contains("unknown"),
        "{json}"
    );
}

#[test]
fn an_optional_exec_gate_that_cannot_be_used_is_reported_once() {
    let f = fake(PDFINFO, PDFTOPPM);
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(f.tools)
        .with_exec_gate(ExecGate::new("/nonexistent/texrun"))
        .render(&fixture("sizes.pdf"), out.path(), &opts())
        .unwrap();
    // Rendered as without a gate (page 2 fails in the fake), with one
    // warning about the limits.
    assert_eq!(report.status, PreviewStatus::Partial);
    assert_eq!(
        kinds(&report),
        vec![NoticeKind::ResourceLimits, NoticeKind::RenderFailed]
    );
    assert!(report.notices[0].message.contains("/nonexistent/texrun"));
}
