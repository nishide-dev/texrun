//! End-to-end tests with the real Poppler / `MuPDF` tools.
//!
//! Each test runs once per backend that is installed on `PATH`. A missing
//! backend is skipped with a message on stderr, so `cargo test` passes on
//! machines (and CI runners) without the tools. Set
//! `TEXRUN_REQUIRE_PREVIEW_TOOLS=1` to turn a missing backend into a failure,
//! as the Docker development environment (which has both) should:
//!
//! ```sh
//! docker compose run --rm -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 dev \
//!     cargo test -p texrun-preview
//! ```
//!
//! Fixtures are built by `tests/fixtures/regenerate.sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use texrun_core::{ArtifactKind, Severity};
use texrun_preview::{
    BackendChoice, BackendKind, NoticeKind, PageRange, PreviewOptions, PreviewReport,
    PreviewStatus, Previewer, Toolset,
};

const REQUIRE_ENV: &str = "TEXRUN_REQUIRE_PREVIEW_TOOLS";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The installed backends, each with options that select it.
fn backends() -> Vec<(Previewer, PreviewOptions)> {
    let tools = Toolset::detect();
    let mut found = Vec::new();
    for (kind, choice) in [
        (BackendKind::Poppler, BackendChoice::Poppler),
        (BackendKind::Mupdf, BackendChoice::Mupdf),
    ] {
        if tools.has(kind) {
            found.push((
                Previewer::new(tools.clone()),
                PreviewOptions::default().with_backend(choice),
            ));
        } else if std::env::var_os(REQUIRE_ENV).is_some_and(|v| v == "1") {
            panic!("{} is not installed but {REQUIRE_ENV}=1", kind.name());
        } else {
            eprintln!(
                "SKIPPED: {} is not installed (set {REQUIRE_ENV}=1 to fail instead)",
                kind.name()
            );
        }
    }
    found
}

fn kinds(report: &PreviewReport) -> Vec<NoticeKind> {
    report.notices.iter().map(|n| n.kind).collect()
}

fn pages(report: &PreviewReport) -> Vec<u32> {
    report
        .pages
        .iter()
        .map(|p| p.artifact.page.unwrap())
        .collect()
}

fn assert_close(actual: (u32, u32), expected: (u32, u32), what: &str) {
    // Tools may round a fractional pixel either way.
    let ok = actual.0.abs_diff(expected.0) <= 1 && actual.1.abs_diff(expected.1) <= 1;
    assert!(ok, "{what}: {actual:?} != {expected:?}");
}

#[test]
fn reads_page_count_sizes_and_rotation() {
    for (previewer, options) in backends() {
        let report = previewer.inspect(&fixture("sizes.pdf"), &options).unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Skipped, "{name}");
        assert!(report.notices.is_empty(), "{name}: {:?}", report.notices);
        let pdf = report.pdf.unwrap();
        assert_eq!(pdf.page_count, 3, "{name}");
        let sizes: Vec<_> = pdf
            .pages
            .iter()
            .map(|p| (p.page, p.width_pt, p.height_pt, p.rotation))
            .collect();
        assert_eq!(
            sizes,
            vec![
                (1, 300.0, 400.0, 0),
                (2, 400.0, 300.0, 0),
                (3, 200.0, 100.0, 90)
            ],
            "{name}"
        );
    }
}

#[test]
fn renders_every_page_to_png() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let report = previewer
            .render(&fixture("sizes.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Rendered, "{name}");
        assert!(report.notices.is_empty(), "{name}: {:?}", report.notices);
        assert_eq!(pages(&report), vec![1, 2, 3], "{name}");

        // 144 DPI = 2 px per pt; page 3 is rotated by 90 degrees.
        let expected = [(600, 800), (800, 600), (200, 400)];
        for (preview, want) in report.pages.iter().zip(expected) {
            let a = &preview.artifact;
            assert_eq!(a.kind, ArtifactKind::Preview);
            let page = a.page.unwrap();
            assert_eq!(a.path.as_str(), format!("preview/page-{page:03}.png"));
            assert_eq!(preview.dpi, 144);
            assert_close((preview.width_px, preview.height_px), want, name);

            let bytes = fs::read(out.path().join(a.path.as_path())).unwrap();
            assert_eq!(a.size_bytes, Some(bytes.len() as u64), "{name}");
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{name}");
        }
        let names: Vec<_> = fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec!["preview"],
            "{name}: scratch directory left behind"
        );
    }
}

#[test]
fn renders_the_first_20_pages_by_default() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let report = previewer
            .render(&fixture("many-pages.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Rendered, "{name}");
        assert_eq!(pages(&report), (1..=20).collect::<Vec<_>>(), "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::PageLimit], "{name}");
        assert_eq!(report.notices[0].severity, Severity::Info);
        let pdf = report.pdf.unwrap();
        assert_eq!((pdf.page_count, pdf.pages.len()), (25, 20), "{name}");
    }
}

#[test]
fn renders_a_requested_page_range() {
    for (previewer, options) in backends() {
        let pdf = fixture("many-pages.pdf");
        let run = |range: &str| {
            let out = tempfile::tempdir().unwrap();
            let range: PageRange = range.parse().unwrap();
            let report = previewer
                .render(&pdf, out.path(), &options.clone().with_pages(range))
                .unwrap();
            (report, out)
        };

        let (report, _out) = run("22-");
        let name = report.backend.unwrap().name();
        assert_eq!(pages(&report), vec![22, 23, 24, 25], "{name}");
        assert!(report.notices.is_empty(), "{name}: {:?}", report.notices);
        assert_eq!(
            report.pages[0].artifact.path.as_str(),
            "preview/page-022.png"
        );

        let (report, _out) = run("24-30");
        assert_eq!(pages(&report), vec![24, 25], "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::PageRangeClamped], "{name}");
        assert_eq!(report.status, PreviewStatus::Rendered, "{name}");

        let (report, _out) = run("26");
        assert_eq!(report.status, PreviewStatus::Skipped, "{name}");
        assert_eq!(
            kinds(&report),
            vec![NoticeKind::PageRangeOutOfBounds],
            "{name}"
        );
        assert_eq!(report.pdf.unwrap().page_count, 25);
    }
}

#[test]
fn a_range_is_capped_at_the_page_limit() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let options = options.with_pages("2-".parse().unwrap()).with_max_pages(3);
        let report = previewer
            .render(&fixture("many-pages.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(pages(&report), vec![2, 3, 4], "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::PageLimit], "{name}");
        assert_eq!(report.notices[0].severity, Severity::Warning);
    }
}

#[test]
fn stops_before_the_total_size_limit() {
    for (previewer, options) in backends() {
        let pdf = fixture("many-pages.pdf");
        let first_two = options.clone().with_pages("1-2".parse().unwrap());
        let out = tempfile::tempdir().unwrap();
        let full = previewer.render(&pdf, out.path(), &first_two).unwrap();
        let name = full.backend.unwrap().name();
        let sizes: Vec<u64> = full
            .pages
            .iter()
            .map(|p| p.artifact.size_bytes.unwrap())
            .collect();

        // Room for page 1 but not for page 2.
        let out = tempfile::tempdir().unwrap();
        let limited = first_two.with_max_total_bytes(sizes[0] + sizes[1] - 1);
        let report = previewer.render(&pdf, out.path(), &limited).unwrap();
        assert_eq!(report.status, PreviewStatus::Partial, "{name}");
        assert_eq!(pages(&report), vec![1], "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::SizeLimit], "{name}");
        assert_eq!(report.notices[0].page, Some(2));
        assert!(!out.path().join("preview/page-002.png").exists(), "{name}");
    }
}

#[test]
fn large_pages_are_rendered_at_a_lower_dpi() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let options = options.with_max_long_edge_px(100);
        let report = previewer
            .render(&fixture("sizes.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Rendered, "{name}");
        for p in &report.pages {
            assert!(p.width_px.max(p.height_px) <= 101, "{name}: {p:?}");
            assert!(p.dpi < 144);
        }
        // floor(100 * 72 / 400) = 18 DPI for the 300 x 400 pt page.
        assert_eq!(report.pages[0].dpi, 18, "{name}");
        assert_close(
            (report.pages[0].width_px, report.pages[0].height_px),
            (75, 100),
            name,
        );
        assert!(
            report
                .notices
                .iter()
                .all(|n| n.kind == NoticeKind::ResolutionReduced && n.severity == Severity::Info),
            "{name}: {:?}",
            report.notices
        );
    }
}

#[test]
fn a_custom_dpi_is_honored() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let options = options
            .with_dpi(72)
            .with_pages(PageRange::single(1).unwrap());
        let report = previewer
            .render(&fixture("sizes.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_close(
            (report.pages[0].width_px, report.pages[0].height_px),
            (300, 400),
            name,
        );
    }
}

#[test]
fn a_corrupt_pdf_is_reported_as_unreadable() {
    for (previewer, options) in backends() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("broken.pdf");
        fs::write(&pdf, b"%PDF-1.7\nthis is not really a PDF\n").unwrap();
        let report = previewer.render(&pdf, dir.path(), &options).unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Skipped, "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::PdfUnreadable], "{name}");
        assert_eq!(report.notices[0].severity, Severity::Warning);
    }
}

#[test]
fn the_timeout_covers_the_whole_run() {
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let options = options.with_timeout(Duration::from_nanos(1));
        let report = previewer
            .render(&fixture("sizes.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        assert_eq!(report.status, PreviewStatus::Skipped, "{name}");
        assert_eq!(kinds(&report), vec![NoticeKind::TimedOut], "{name}");
    }
}

#[test]
fn metadata_in_the_pdf_cannot_spoof_page_count_or_size() {
    // The title of this fixture contains fake `Pages:` / `Page 1 size:` lines.
    for (previewer, options) in backends() {
        let out = tempfile::tempdir().unwrap();
        let report = previewer
            .render(&fixture("spoofed-metadata.pdf"), out.path(), &options)
            .unwrap();
        let name = report.backend.unwrap().name();
        let pdf = report.pdf.as_ref().unwrap();
        assert_eq!(pdf.page_count, 1, "{name}");
        let page = pdf.pages[0];
        assert_eq!(
            (page.width_pt, page.height_pt, page.rotation),
            (100.0, 50.0, 0),
            "{name}"
        );
        assert_eq!(report.status, PreviewStatus::Rendered, "{name}");
        assert_close(
            (report.pages[0].width_px, report.pages[0].height_px),
            (200, 100),
            name,
        );
    }
}

#[test]
fn auto_prefers_mupdf() {
    let tools = Toolset::detect();
    let out = tempfile::tempdir().unwrap();
    let report = Previewer::new(tools.clone())
        .render(
            &fixture("sizes.pdf"),
            out.path(),
            &PreviewOptions::default(),
        )
        .unwrap();
    // `None` (and a `tool_unavailable` notice) when nothing is installed.
    assert_eq!(report.backend, tools.available().first().copied());
    if tools.has(BackendKind::Mupdf) {
        assert_eq!(report.backend, Some(BackendKind::Mupdf));
    }
}
