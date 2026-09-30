//! Results of a preview run: PDF metadata, page images and notices.

use serde::{Deserialize, Serialize};
use texrun_core::{Artifact, CompileResult, Severity};

/// The external tool family that produced a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackendKind {
    /// Poppler: `pdfinfo` for metadata, `pdftoppm` for rendering.
    Poppler,
    /// `MuPDF`: `mutool info` / `mutool pages` for metadata, `mutool draw` for
    /// rendering.
    Mupdf,
}

impl BackendKind {
    /// Stable lower-case name (`poppler` / `mupdf`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Poppler => "poppler",
            Self::Mupdf => "mupdf",
        }
    }
}

/// Image format of the previews. Only PNG today: every common vision API
/// accepts it and both backends write it directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImageFormat {
    /// `image/png`.
    #[default]
    Png,
}

impl ImageFormat {
    /// The media type, e.g. `image/png`.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
        }
    }
}

/// Metadata of the PDF, as reported by the backend.
///
/// The PDF is untrusted input; the values are what the file declares and are
/// only used for display and for bounding the rendering work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PdfInfo {
    /// Number of pages in the document.
    pub page_count: u32,
    /// Sizes of the *selected* pages (the ones a preview was attempted for),
    /// in page order. Not every page of a long document is listed.
    ///
    /// The point values depend on the backend for pages with a `UserUnit`
    /// (a scale factor for the page's units): `MuPDF` reports the size with
    /// the factor applied (the size it renders at), Poppler (`pdfinfo`)
    /// reports the unscaled page box. For all other pages both agree.
    #[serde(default)]
    pub pages: Vec<PageInfo>,
}

/// Size and orientation of one page.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PageInfo {
    /// 1-based page number.
    pub page: u32,
    /// Width of the page box in PDF points (1/72 in), before rotation.
    pub width_pt: f64,
    /// Height of the page box in PDF points, before rotation.
    pub height_pt: f64,
    /// Clockwise display rotation: 0, 90, 180 or 270.
    #[serde(default)]
    pub rotation: u16,
}

impl PageInfo {
    pub(crate) fn new(page: u32, width_pt: f64, height_pt: f64, rotation: u16) -> Self {
        Self {
            page,
            width_pt,
            height_pt,
            rotation,
        }
    }

    /// The longer side in points (independent of rotation).
    pub fn long_edge_pt(&self) -> f64 {
        self.width_pt.max(self.height_pt)
    }
}

/// One rendered page: the [`Artifact`] plus image details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PagePreview {
    /// The image, kind [`ArtifactKind::Preview`](texrun_core::ArtifactKind::Preview),
    /// with `page` and `size_bytes` set. The path is relative to the output
    /// root (e.g. `preview/page-001.png`).
    #[serde(flatten)]
    pub artifact: Artifact,
    /// Image width in pixels (after rotation).
    pub width_px: u32,
    /// Image height in pixels (after rotation).
    pub height_px: u32,
    /// Resolution the page was rendered at.
    pub dpi: u32,
}

/// Overall result of a preview run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PreviewStatus {
    /// Every selected page was rendered.
    Rendered,
    /// Some selected pages were rendered, others were not (see the notices).
    Partial,
    /// No page was rendered (tool missing, unreadable PDF, limits, ...; see
    /// the notices).
    Skipped,
    /// Metadata only ([`Previewer::inspect`](crate::Previewer::inspect)):
    /// [`PreviewReport::pdf`] was read and nothing was meant to be rendered.
    Inspected,
}

/// Why a [`PreviewNotice`] was issued. Consumers must treat unknown values
/// like [`NoticeKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeKind {
    /// No usable preview tool is installed.
    ToolUnavailable,
    /// The PDF could not be opened or its metadata could not be read.
    PdfUnreadable,
    /// Only part of the document was selected because of a page limit.
    PageLimit,
    /// The requested range ends after the last page and was shortened.
    PageRangeClamped,
    /// The requested range starts after the last page; nothing was rendered.
    PageRangeOutOfBounds,
    /// A page was rendered at a lower DPI to respect the pixel limit.
    ResolutionReduced,
    /// The tool failed on a page.
    RenderFailed,
    /// Rendering stopped because the total image size limit was reached.
    SizeLimit,
    /// Rendering stopped at the wall-clock limit.
    TimedOut,
    /// Rendering stopped because cancellation was requested.
    Cancelled,
    /// The images could not be written to the output root.
    OutputError,
    /// The resource limits of the tools could not be set before they
    /// started, because the exec gate cannot be used
    /// ([`Previewer::with_exec_gate`](crate::Previewer::with_exec_gate)):
    /// either nothing was rendered (a required gate), or the limits were
    /// set only after each tool started (best effort).
    ResourceLimits,
    /// Anything else. Unknown values are deserialized as this variant.
    #[serde(other)]
    Other,
}

/// A problem or remark about a preview run. Not a compile diagnostic: a
/// preview notice never means that the document failed to compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreviewNotice {
    /// [`Severity::Info`] for expected limitations (page limit, reduced
    /// resolution), [`Severity::Warning`] when requested previews are missing.
    pub severity: Severity,
    /// Classification.
    pub kind: NoticeKind,
    /// Human-readable message.
    pub message: String,
    /// The page concerned, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// Excerpt of the tool's stderr, with control characters escaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PreviewNotice {
    pub(crate) fn new(severity: Severity, kind: NoticeKind, message: impl Into<String>) -> Self {
        Self {
            severity,
            kind,
            message: message.into(),
            page: None,
            detail: None,
        }
    }

    pub(crate) fn warning(kind: NoticeKind, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, kind, message)
    }

    pub(crate) fn info(kind: NoticeKind, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, kind, message)
    }

    #[must_use]
    pub(crate) fn with_page(mut self, page: u32) -> Self {
        self.page = Some(page);
        self
    }

    #[must_use]
    pub(crate) fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail.filter(|d| !d.is_empty());
        self
    }
}

/// The result of [`Previewer::render`](crate::Previewer::render) /
/// [`Previewer::inspect`](crate::Previewer::inspect).
///
/// Serializable so that the CLI (#6) can embed it next to the
/// [`CompileResult`] in its JSON output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreviewReport {
    /// Overall result.
    pub status: PreviewStatus,
    /// The backend used, if one was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendKind>,
    /// Image format of [`PreviewReport::pages`].
    #[serde(default)]
    pub format: ImageFormat,
    /// PDF metadata, if it could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfInfo>,
    /// Rendered pages, in page order.
    #[serde(default)]
    pub pages: Vec<PagePreview>,
    /// Problems and remarks, in the order they occurred.
    #[serde(default)]
    pub notices: Vec<PreviewNotice>,
}

impl PreviewReport {
    pub(crate) fn empty() -> Self {
        Self {
            status: PreviewStatus::Skipped,
            backend: None,
            format: ImageFormat::Png,
            pdf: None,
            pages: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// The preview artifacts, in page order.
    pub fn artifacts(&self) -> impl Iterator<Item = &Artifact> {
        self.pages.iter().map(|p| &p.artifact)
    }

    /// Appends the preview artifacts to `result.artifacts`, so that they are
    /// reported (and collected by the workspace layer) like the PDF and log.
    pub fn attach_to(&self, result: &mut CompileResult) {
        result.artifacts.extend(self.artifacts().cloned());
    }

    /// Notices with [`Severity::Warning`] or worse.
    pub fn warnings(&self) -> impl Iterator<Item = &PreviewNotice> {
        self.notices
            .iter()
            .filter(|n| n.severity >= Severity::Warning)
    }

    pub(crate) fn push(&mut self, notice: PreviewNotice) {
        self.notices.push(notice);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use texrun_core::{ArtifactKind, CompileOutcome, EngineInfo, WorkspacePath};

    use super::*;

    fn sample() -> PreviewReport {
        let mut r = PreviewReport::empty();
        r.status = PreviewStatus::Partial;
        r.backend = Some(BackendKind::Poppler);
        r.pdf = Some(PdfInfo {
            page_count: 30,
            pages: vec![PageInfo::new(1, 595.0, 842.0, 0)],
        });
        r.pages.push(PagePreview {
            artifact: Artifact::new(
                ArtifactKind::Preview,
                WorkspacePath::new("preview/page-001.png").unwrap(),
            )
            .with_page(1)
            .with_size_bytes(1234),
            width_px: 1190,
            height_px: 1684,
            dpi: 144,
        });
        r.push(PreviewNotice::info(
            NoticeKind::PageLimit,
            "first 20 of 30 pages",
        ));
        r.push(
            PreviewNotice::warning(NoticeKind::RenderFailed, "boom")
                .with_page(2)
                .with_detail(Some("Syntax Error".to_owned())),
        );
        r
    }

    #[test]
    fn json_shape_and_round_trip() {
        let r = sample();
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json,
            json!({
                "status": "partial",
                "backend": "poppler",
                "format": "png",
                "pdf": {
                    "page_count": 30,
                    "pages": [ { "page": 1, "width_pt": 595.0, "height_pt": 842.0, "rotation": 0 } ]
                },
                "pages": [ {
                    "kind": "preview", "path": "preview/page-001.png", "page": 1,
                    "size_bytes": 1234, "width_px": 1190, "height_px": 1684, "dpi": 144
                } ],
                "notices": [
                    { "severity": "info", "kind": "page_limit", "message": "first 20 of 30 pages" },
                    { "severity": "warning", "kind": "render_failed", "message": "boom",
                      "page": 2, "detail": "Syntax Error" }
                ]
            })
        );
        assert_eq!(serde_json::from_value::<PreviewReport>(json).unwrap(), r);
    }

    #[test]
    fn unknown_notice_kind_deserializes_as_other() {
        let n: PreviewNotice = serde_json::from_value(
            json!({ "severity": "warning", "kind": "something_new", "message": "x" }),
        )
        .unwrap();
        assert_eq!(n.kind, NoticeKind::Other);
    }

    #[test]
    fn attaches_preview_artifacts_to_a_compile_result() {
        let r = sample();
        let mut result = CompileResult::new(
            CompileOutcome::Succeeded,
            EngineInfo::new("fake"),
            Duration::from_millis(1),
        );
        r.attach_to(&mut result);
        let previews: Vec<_> = result.artifacts_of(ArtifactKind::Preview).collect();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].path.as_str(), "preview/page-001.png");
        assert_eq!(previews[0].page, Some(1));
        // Attaching previews never changes the compile outcome.
        assert!(result.is_success());
    }

    #[test]
    fn warnings_skip_info_notices() {
        let r = sample();
        let kinds: Vec<_> = r.warnings().map(|n| n.kind).collect();
        assert_eq!(kinds, vec![NoticeKind::RenderFailed]);
        assert_eq!(ImageFormat::Png.media_type(), "image/png");
        assert_eq!(BackendKind::Mupdf.name(), "mupdf");
    }
}
