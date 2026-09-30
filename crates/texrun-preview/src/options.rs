//! Preview options, page ranges and limits.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use texrun_core::{CancelToken, WorkspacePath};

use crate::error::PreviewError;

/// Default rendering resolution in dots per inch.
///
/// 144 DPI is twice the PDF unit (1 pt = 1/72 in), so every PDF length maps to
/// a whole number of pixels. An A4 page becomes 1191 x 1684 px (about 2 MP),
/// which is at or slightly above the input size current vision models work at
/// (for example about 1568 px on the long edge before down-scaling), so more
/// pixels would mostly cost bytes and tokens. 10 pt body text is about 20 px
/// high, enough for glyphs, sub/superscripts and table rules to stay legible.
pub const DEFAULT_DPI: u32 = 144;

/// Highest accepted [`PreviewOptions::dpi`].
pub const MAX_DPI: u32 = 1200;

/// Pages rendered when no range is given: the first 20 (docs/security.md §3.2).
pub const DEFAULT_PAGE_LIMIT: u32 = 20;

/// Pages rendered at most, even when a range is given (docs/security.md §3.2).
pub const MAX_PAGE_LIMIT: u32 = 200;

/// Total size of all preview images of one run: 128 MiB (docs/security.md §3.2).
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

/// Wall-clock limit for one preview run, all pages together
/// (docs/security.md §3.1).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest edge of one preview image in pixels. Pages that would be larger at
/// the requested DPI are rendered at a lower DPI (reported as
/// [`NoticeKind::ResolutionReduced`](crate::NoticeKind::ResolutionReduced)).
/// This bounds the memory and output size of a single page regardless of the
/// page size declared by the (untrusted) PDF.
pub const DEFAULT_MAX_LONG_EDGE_PX: u32 = 4096;

/// Which external tool renders the previews.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackendChoice {
    /// `MuPDF` if installed, otherwise Poppler.
    ///
    /// The choice depends only on which tools are installed. If the chosen
    /// tool fails on a PDF (`pdf_unreadable`, `render_failed`), the other one
    /// is not tried; select it explicitly to compare.
    #[default]
    Auto,
    /// Poppler (`pdfinfo` + `pdftoppm`) only.
    Poppler,
    /// `MuPDF` (`mutool`) only.
    Mupdf,
}

/// A 1-based, inclusive page range. `last == None` means "to the last page".
///
/// Parsed from `N`, `N-M`, `N-` or `-M` (see [`FromStr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageRange {
    first: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<u32>,
}

/// An invalid [`PageRange`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid page range {input:?}: {reason}")]
pub struct PageRangeError {
    input: String,
    reason: &'static str,
}

impl PageRange {
    /// Creates a range from `first` to `last` (inclusive, 1-based).
    pub fn new(first: u32, last: Option<u32>) -> Result<Self, PageRangeError> {
        let err = |reason| PageRangeError {
            input: match last {
                Some(last) => format!("{first}-{last}"),
                None => format!("{first}-"),
            },
            reason,
        };
        if first == 0 {
            return Err(err("pages are numbered from 1"));
        }
        if last.is_some_and(|last| last < first) {
            return Err(err("the last page is before the first page"));
        }
        Ok(Self { first, last })
    }

    /// A single page.
    pub fn single(page: u32) -> Result<Self, PageRangeError> {
        Self::new(page, Some(page))
    }

    /// The first page.
    pub fn first(&self) -> u32 {
        self.first
    }

    /// The last page, or `None` for "to the end".
    pub fn last(&self) -> Option<u32> {
        self.last
    }
}

impl FromStr for PageRange {
    type Err = PageRangeError;

    /// Accepts `N`, `N-M`, `N-` and `-M` (surrounding whitespace is ignored).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason| PageRangeError {
            input: s.to_owned(),
            reason,
        };
        let number = |text: &str| -> Result<u32, PageRangeError> {
            let text = text.trim();
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(err("expected N, N-M, N- or -M"));
            }
            text.parse().map_err(|_| err("page number is too large"))
        };
        let (first, last) = match s.trim().split_once('-') {
            None => {
                let page = number(s)?;
                (page, Some(page))
            }
            Some((first, last)) => {
                let first = if first.trim().is_empty() {
                    1
                } else {
                    number(first)?
                };
                let last = if last.trim().is_empty() {
                    None
                } else {
                    Some(number(last)?)
                };
                (first, last)
            }
        };
        Self::new(first, last).map_err(|e| PageRangeError {
            input: s.to_owned(),
            reason: e.reason,
        })
    }
}

impl fmt::Display for PageRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.last {
            Some(last) if last == self.first => write!(f, "{last}"),
            Some(last) => write!(f, "{}-{last}", self.first),
            None => write!(f, "{}-", self.first),
        }
    }
}

/// Options for one preview run.
///
/// `#[non_exhaustive]`: start from [`PreviewOptions::default`] and use the
/// `with_*` methods. The defaults are the limits of docs/security.md §3.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PreviewOptions {
    /// Which tool to use.
    pub backend: BackendChoice,
    /// Requested resolution. Very large pages are rendered at a lower DPI,
    /// see [`PreviewOptions::max_long_edge_px`].
    pub dpi: u32,
    /// Pages to render; `None` renders the first
    /// [`PreviewOptions::default_page_limit`] pages.
    pub pages: Option<PageRange>,
    /// Pages rendered when [`PreviewOptions::pages`] is `None`.
    pub default_page_limit: u32,
    /// Upper bound on the number of rendered pages for any selection.
    pub max_pages: u32,
    /// Upper bound on the total size of all images. Rendering stops before the
    /// page that would exceed it.
    pub max_total_bytes: u64,
    /// Upper bound on the long edge of one image in pixels.
    pub max_long_edge_px: u32,
    /// Wall-clock limit for the whole run (metadata and all pages).
    pub timeout: Duration,
    /// Cancellation flag, polled while tools run.
    pub cancel: CancelToken,
    /// Directory for the images, relative to the output root.
    pub output_subdir: WorkspacePath,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            backend: BackendChoice::Auto,
            dpi: DEFAULT_DPI,
            pages: None,
            default_page_limit: DEFAULT_PAGE_LIMIT,
            max_pages: MAX_PAGE_LIMIT,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_long_edge_px: DEFAULT_MAX_LONG_EDGE_PX,
            timeout: DEFAULT_TIMEOUT,
            cancel: CancelToken::new(),
            output_subdir: WorkspacePath::new("preview").expect("valid literal path"),
        }
    }
}

impl PreviewOptions {
    /// Sets the backend.
    #[must_use]
    pub fn with_backend(mut self, backend: BackendChoice) -> Self {
        self.backend = backend;
        self
    }

    /// Sets the resolution.
    #[must_use]
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self
    }

    /// Renders only `pages` (still capped at [`PreviewOptions::max_pages`]).
    #[must_use]
    pub fn with_pages(mut self, pages: PageRange) -> Self {
        self.pages = Some(pages);
        self
    }

    /// Sets the number of pages rendered when no range is given.
    #[must_use]
    pub fn with_default_page_limit(mut self, pages: u32) -> Self {
        self.default_page_limit = pages;
        self
    }

    /// Sets the upper bound on the number of rendered pages.
    #[must_use]
    pub fn with_max_pages(mut self, pages: u32) -> Self {
        self.max_pages = pages;
        self
    }

    /// Sets the total image size limit.
    #[must_use]
    pub fn with_max_total_bytes(mut self, bytes: u64) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    /// Sets the per-image long edge limit.
    #[must_use]
    pub fn with_max_long_edge_px(mut self, px: u32) -> Self {
        self.max_long_edge_px = px;
        self
    }

    /// Sets the wall-clock limit.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Uses `cancel` as the cancellation flag.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets the image directory (relative to the output root).
    #[must_use]
    pub fn with_output_subdir(mut self, dir: WorkspacePath) -> Self {
        self.output_subdir = dir;
        self
    }

    /// Checks that the options are usable. Called by every entry point.
    pub fn validate(&self) -> Result<(), PreviewError> {
        let invalid = |msg: String| Err(PreviewError::InvalidOptions(msg));
        if !(1..=MAX_DPI).contains(&self.dpi) {
            return invalid(format!("dpi must be between 1 and {MAX_DPI}"));
        }
        if self.default_page_limit == 0 || self.max_pages == 0 {
            return invalid("page limits must be at least 1".to_owned());
        }
        if self.max_long_edge_px == 0 {
            return invalid("max_long_edge_px must be at least 1".to_owned());
        }
        if self.timeout.is_zero() {
            return invalid("timeout must be greater than zero".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(s: &str) -> PageRange {
        s.parse().unwrap()
    }

    #[test]
    fn parses_page_ranges() {
        assert_eq!(range("3"), PageRange::single(3).unwrap());
        assert_eq!(range("2-5"), PageRange::new(2, Some(5)).unwrap());
        assert_eq!(range(" 2 - 5 "), PageRange::new(2, Some(5)).unwrap());
        assert_eq!(range("4-"), PageRange::new(4, None).unwrap());
        assert_eq!(range("-4"), PageRange::new(1, Some(4)).unwrap());
        assert_eq!(range("-"), PageRange::new(1, None).unwrap());
    }

    #[test]
    fn rejects_bad_page_ranges() {
        for bad in [
            "",
            "0",
            "0-3",
            "5-2",
            "a",
            "1-b",
            "+1",
            "1-2-3",
            "1,2",
            "99999999999",
        ] {
            assert!(bad.parse::<PageRange>().is_err(), "{bad:?}");
        }
        let err = "5-2".parse::<PageRange>().unwrap_err();
        assert!(err.to_string().contains("\"5-2\""), "{err}");
    }

    #[test]
    fn page_range_display_round_trips() {
        for text in ["3", "2-5", "4-"] {
            assert_eq!(range(text).to_string(), text);
            assert_eq!(range(&range(text).to_string()), range(text));
        }
    }

    #[test]
    fn defaults_follow_the_security_model() {
        let o = PreviewOptions::default();
        assert_eq!(o.dpi, 144);
        assert_eq!(o.default_page_limit, 20);
        assert_eq!(o.max_pages, 200);
        assert_eq!(o.max_total_bytes, 128 * 1024 * 1024);
        assert_eq!(o.timeout, Duration::from_secs(30));
        assert_eq!(o.output_subdir.as_str(), "preview");
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unusable_options() {
        let base = PreviewOptions::default;
        for bad in [
            base().with_dpi(0),
            base().with_dpi(MAX_DPI + 1),
            base().with_max_pages(0),
            base().with_default_page_limit(0),
            base().with_max_long_edge_px(0),
            base().with_timeout(Duration::ZERO),
        ] {
            assert!(
                matches!(bad.validate(), Err(PreviewError::InvalidOptions(_))),
                "{bad:?}"
            );
        }
    }
}
