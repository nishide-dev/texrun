//! The preview run: metadata, page selection and rendering.

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use texrun_core::{Artifact, ArtifactKind, WorkspacePath};

use crate::backend::Invocation;
use crate::error::PreviewError;
use crate::options::PreviewOptions;
use crate::png;
use crate::process::{self, Limits, RunEnd, RunOutput, ToolEnv};
use crate::report::{
    NoticeKind, PageInfo, PagePreview, PdfInfo, PreviewNotice, PreviewReport, PreviewStatus,
};
use crate::tools::{Backend, Toolset};

/// Characters of tool stderr kept in a notice.
const DETAIL_CHARS: usize = 2000;
/// File name the tool renders to, inside the private working directory.
const RENDER_NAME: &str = "page.png";

/// Generates page previews and reads PDF metadata with the external tools of
/// a [`Toolset`].
#[derive(Debug, Clone)]
pub struct Previewer {
    tools: Toolset,
}

impl Previewer {
    /// Uses the tools of `tools`.
    pub fn new(tools: Toolset) -> Self {
        Self { tools }
    }

    /// Uses the tools found on this process's `PATH`.
    pub fn detect() -> Self {
        Self::new(Toolset::detect())
    }

    /// The tools in use.
    pub fn toolset(&self) -> &Toolset {
        &self.tools
    }

    /// Reads the page count and the sizes of the selected pages of `pdf`
    /// without rendering anything. The report's status is always
    /// [`PreviewStatus::Skipped`] and [`PreviewReport::pdf`] is set on success.
    pub fn inspect(
        &self,
        pdf: &Path,
        options: &PreviewOptions,
    ) -> Result<PreviewReport, PreviewError> {
        self.run(pdf, None, options)
    }

    /// Renders the selected pages of `pdf` to PNG files in
    /// `<output_root>/<options.output_subdir>/page-NNN.png` (page number
    /// zero-padded to three digits) and returns them as
    /// [`ArtifactKind::Preview`] artifacts relative to `output_root`.
    ///
    /// `pdf` is a host path (normally the PDF artifact inside the engine's
    /// output directory) and `output_root` the directory that artifact paths
    /// are relative to. Existing files with the same names are replaced.
    ///
    /// Only invalid `options` are an error. Missing tools, unreadable PDFs,
    /// tool failures and limits are reported as notices, so a caller can
    /// always keep the compile result and add the report to it.
    pub fn render(
        &self,
        pdf: &Path,
        output_root: &Path,
        options: &PreviewOptions,
    ) -> Result<PreviewReport, PreviewError> {
        self.run(pdf, Some(output_root), options)
    }

    fn run(
        &self,
        pdf: &Path,
        output_root: Option<&Path>,
        options: &PreviewOptions,
    ) -> Result<PreviewReport, PreviewError> {
        options.validate()?;
        let mut report = PreviewReport::empty();
        let backend = match self.tools.select(options.backend) {
            Ok(backend) => backend,
            Err(msg) => {
                report.push(PreviewNotice::warning(NoticeKind::ToolUnavailable, msg));
                return Ok(report);
            }
        };
        report.backend = Some(backend.kind());
        let mut run = Run {
            backend,
            options,
            deadline: Instant::now() + options.timeout,
            report,
            selected: 0,
        };
        run.execute(pdf, output_root, self.tools.search_path());
        Ok(run.finish())
    }
}

struct Run<'a> {
    backend: Backend<'a>,
    options: &'a PreviewOptions,
    deadline: Instant,
    report: PreviewReport,
    selected: u32,
}

impl Run<'_> {
    fn execute(
        &mut self,
        pdf: &Path,
        output_root: Option<&Path>,
        search_path: Option<&std::ffi::OsStr>,
    ) {
        let Some(pdf) = self.check_pdf(pdf) else {
            return;
        };
        // Private scratch space: `HOME` for the tools and their working
        // directory. For rendering it lives in the output root so that
        // finished images can be renamed into place.
        let scratch = match output_root {
            Some(root) => tempfile::Builder::new()
                .prefix(".texrun-preview-")
                .tempdir_in(root),
            None => tempfile::Builder::new().prefix("texrun-preview-").tempdir(),
        };
        let scratch = match scratch.and_then(|dir| {
            fs::create_dir(dir.path().join("home"))?;
            fs::create_dir(dir.path().join("work"))?;
            Ok(dir)
        }) {
            Ok(dir) => dir,
            Err(e) => {
                self.notice(PreviewNotice::warning(
                    NoticeKind::OutputError,
                    format!("cannot create a scratch directory for the preview tools: {e}"),
                ));
                return;
            }
        };
        let env = ToolEnv {
            path: search_path.map(std::ffi::OsStr::to_os_string),
            home: scratch.path().join("home"),
            cwd: scratch.path().join("work"),
        };

        let Some(count) = self.page_count(&pdf, &env) else {
            return;
        };
        self.report.pdf = Some(PdfInfo {
            page_count: count,
            pages: Vec::new(),
        });
        let Some((first, last)) = select_pages(count, self.options, &mut self.report.notices)
        else {
            return;
        };
        let Some(pages) = self.page_sizes(&pdf, &env, first, last) else {
            return;
        };
        if let Some(info) = self.report.pdf.as_mut() {
            info.pages.clone_from(&pages);
        }
        if let Some(root) = output_root {
            self.selected = last - first + 1;
            self.render_pages(&pdf, root, &env, first, last, &pages);
        }
    }

    fn finish(mut self) -> PreviewReport {
        let rendered = u32::try_from(self.report.pages.len()).unwrap_or(u32::MAX);
        self.report.status = if rendered == 0 {
            PreviewStatus::Skipped
        } else if rendered == self.selected {
            PreviewStatus::Rendered
        } else {
            PreviewStatus::Partial
        };
        self.report
    }

    fn notice(&mut self, notice: PreviewNotice) {
        self.report.push(notice);
    }

    fn check_pdf(&mut self, pdf: &Path) -> Option<PathBuf> {
        // Absolute, so that the argument can never be taken for an option.
        let checked = std::path::absolute(pdf).and_then(|abs| {
            if fs::metadata(&abs)?.is_file() {
                Ok(abs)
            } else {
                Err(io::Error::other("not a regular file"))
            }
        });
        match checked {
            Ok(abs) => Some(abs),
            Err(e) => {
                self.notice(PreviewNotice::warning(
                    NoticeKind::PdfUnreadable,
                    format!("cannot open the PDF: {e}"),
                ));
                None
            }
        }
    }

    fn invoke(
        &self,
        inv: &Invocation<'_>,
        env: &ToolEnv,
        watch: Option<(&Path, u64)>,
    ) -> RunOutput {
        process::run(
            inv.program,
            &inv.args,
            env,
            watch.is_none(),
            Limits {
                deadline: self.deadline,
                cancel: &self.options.cancel,
                watch,
            },
        )
    }

    /// Turns a run that did not exit on its own into a notice. Returns `true`
    /// if the caller should stop.
    fn stopped(&mut self, inv: &Invocation<'_>, out: &RunOutput, page: Option<u32>) -> bool {
        let notice = match &out.end {
            RunEnd::Exited(_) => return false,
            RunEnd::TimedOut => PreviewNotice::warning(
                NoticeKind::TimedOut,
                format!(
                    "preview generation exceeded its time limit of {:?}",
                    self.options.timeout
                ),
            ),
            RunEnd::Cancelled => {
                PreviewNotice::warning(NoticeKind::Cancelled, "preview generation was cancelled")
            }
            RunEnd::OutputTooLarge => PreviewNotice::warning(
                NoticeKind::SizeLimit,
                format!(
                    "the preview images would exceed the size limit of {} bytes",
                    self.options.max_total_bytes
                ),
            ),
            RunEnd::Failed(e) => PreviewNotice::warning(
                NoticeKind::ToolUnavailable,
                format!("cannot run {}: {e}", program_name(inv)),
            ),
        };
        self.notice(match page {
            Some(page) => notice.with_page(page),
            None => notice,
        });
        true
    }

    fn unreadable(&mut self, message: String, out: &RunOutput) {
        self.notice(
            PreviewNotice::warning(NoticeKind::PdfUnreadable, message)
                .with_detail(process::excerpt(&out.stderr, DETAIL_CHARS)),
        );
    }

    fn page_count(&mut self, pdf: &Path, env: &ToolEnv) -> Option<u32> {
        let inv = self.backend.page_count(pdf);
        let out = self.invoke(&inv, env, None);
        if self.stopped(&inv, &out, None) {
            return None;
        }
        if !out.succeeded() {
            self.unreadable(format!("{} cannot read the PDF", program_name(&inv)), &out);
            return None;
        }
        match Backend::parse_page_count(&String::from_utf8_lossy(&out.stdout)) {
            Some(0) => {
                self.unreadable("the PDF has no pages".to_owned(), &out);
                None
            }
            Some(count) => Some(count),
            None => {
                self.unreadable(
                    format!("{} did not report a page count", program_name(&inv)),
                    &out,
                );
                None
            }
        }
    }

    fn page_sizes(
        &mut self,
        pdf: &Path,
        env: &ToolEnv,
        first: u32,
        last: u32,
    ) -> Option<Vec<PageInfo>> {
        let inv = self.backend.page_sizes(pdf, first, last);
        let out = self.invoke(&inv, env, None);
        if self.stopped(&inv, &out, None) {
            return None;
        }
        if !out.succeeded() {
            self.unreadable(
                format!("{} cannot read the page sizes", program_name(&inv)),
                &out,
            );
            return None;
        }
        Some(
            self.backend
                .parse_page_sizes(&String::from_utf8_lossy(&out.stdout), first, last),
        )
    }

    fn render_pages(
        &mut self,
        pdf: &Path,
        output_root: &Path,
        env: &ToolEnv,
        first: u32,
        last: u32,
        pages: &[PageInfo],
    ) {
        let dir = match prepare_dir(output_root, &self.options.output_subdir) {
            Ok(dir) => dir,
            Err(e) => {
                self.notice(PreviewNotice::warning(
                    NoticeKind::OutputError,
                    format!(
                        "cannot create the preview directory {:?}: {e}",
                        self.options.output_subdir.as_str()
                    ),
                ));
                return;
            }
        };
        let mut total: u64 = 0;
        for page in first..=last {
            let Some(info) = pages.iter().find(|p| p.page == page) else {
                self.notice(
                    PreviewNotice::warning(
                        NoticeKind::RenderFailed,
                        format!("the size of page {page} is unknown; not rendered"),
                    )
                    .with_page(page),
                );
                continue;
            };
            if self
                .render_page(pdf, env, &dir, info, &mut total)
                .is_break()
            {
                break;
            }
        }
    }

    /// Renders one page into `dir`. `Break` stops the whole run (timeout,
    /// cancellation, size limit, output error); a failure of this page only
    /// is reported and returns `Continue`.
    fn render_page(
        &mut self,
        pdf: &Path,
        env: &ToolEnv,
        dir: &Path,
        info: &PageInfo,
        total: &mut u64,
    ) -> ControlFlow<()> {
        let page = info.page;
        let Some(dpi) = effective_dpi(self.options, info) else {
            self.notice(
                PreviewNotice::warning(
                    NoticeKind::RenderFailed,
                    format!(
                        "page {page} is too large to render ({} x {} pt)",
                        info.width_pt, info.height_pt
                    ),
                )
                .with_page(page),
            );
            return ControlFlow::Continue(());
        };
        if dpi < self.options.dpi {
            self.notice(
                PreviewNotice::info(
                    NoticeKind::ResolutionReduced,
                    format!(
                        "page {page} rendered at {dpi} DPI instead of {} to stay within {} px",
                        self.options.dpi, self.options.max_long_edge_px
                    ),
                )
                .with_page(page),
            );
        }

        let tmp = env.cwd.join(RENDER_NAME);
        let budget = self.options.max_total_bytes.saturating_sub(*total);
        let _ = fs::remove_file(&tmp);
        let inv = self.backend.render(pdf, page, dpi, RENDER_NAME);
        let out = self.invoke(&inv, env, Some((&tmp, budget)));
        if self.stopped(&inv, &out, Some(page)) {
            return ControlFlow::Break(());
        }
        if !out.succeeded() {
            self.render_failed(page, &format!("{} failed", program_name(&inv)), &out);
            return ControlFlow::Continue(());
        }
        let size = match fs::symlink_metadata(&tmp) {
            Ok(meta) if meta.is_file() => meta.len(),
            _ => {
                self.render_failed(page, "no image was written", &out);
                return ControlFlow::Continue(());
            }
        };
        if size > budget {
            self.notice(
                PreviewNotice::warning(
                    NoticeKind::SizeLimit,
                    format!(
                        "the preview images would exceed the size limit of {} bytes",
                        self.options.max_total_bytes
                    ),
                )
                .with_page(page),
            );
            return ControlFlow::Break(());
        }
        let Some((width_px, height_px)) = png::file_dimensions(&tmp) else {
            self.render_failed(page, "the output is not a PNG image", &out);
            return ControlFlow::Continue(());
        };
        let name = WorkspacePath::new(&format!("page-{page:03}.png"))
            .expect("generated file name is a valid path");
        if let Err(e) = fs::rename(&tmp, dir.join(name.as_str())) {
            self.notice(
                PreviewNotice::warning(
                    NoticeKind::OutputError,
                    format!("cannot store the image of page {page}: {e}"),
                )
                .with_page(page),
            );
            return ControlFlow::Break(());
        }
        *total += size;
        self.report.pages.push(PagePreview {
            artifact: Artifact::new(
                ArtifactKind::Preview,
                self.options.output_subdir.join(&name),
            )
            .with_page(page)
            .with_size_bytes(size),
            width_px,
            height_px,
            dpi,
        });
        ControlFlow::Continue(())
    }

    fn render_failed(&mut self, page: u32, what: &str, out: &RunOutput) {
        self.notice(
            PreviewNotice::warning(
                NoticeKind::RenderFailed,
                format!("cannot render page {page}: {what}"),
            )
            .with_page(page)
            .with_detail(process::excerpt(&out.stderr, DETAIL_CHARS)),
        );
    }
}

fn program_name(inv: &Invocation<'_>) -> String {
    inv.program.file_name().map_or_else(
        || "the preview tool".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Chooses the pages to preview, adding notices for any shortening. `None`
/// if nothing is selected.
pub(crate) fn select_pages(
    count: u32,
    options: &PreviewOptions,
    notices: &mut Vec<PreviewNotice>,
) -> Option<(u32, u32)> {
    let Some(range) = options.pages else {
        let limit = options.default_page_limit.min(options.max_pages);
        let last = count.min(limit);
        if count > last {
            notices.push(PreviewNotice::info(
                NoticeKind::PageLimit,
                format!(
                    "previewing pages 1-{last} of {count}; request a page range to preview \
                     other pages"
                ),
            ));
        }
        return Some((1, last));
    };
    let first = range.first();
    if first > count {
        notices.push(PreviewNotice::warning(
            NoticeKind::PageRangeOutOfBounds,
            format!("page range {range} starts after the last page ({count}); nothing to preview"),
        ));
        return None;
    }
    let mut last = range.last().unwrap_or(count);
    if last > count {
        notices.push(PreviewNotice::info(
            NoticeKind::PageRangeClamped,
            format!("page range {range} ends after the last page; previewing {first}-{count}"),
        ));
        last = count;
    }
    let span = last - first + 1;
    if span > options.max_pages {
        let capped = first + (options.max_pages - 1);
        notices.push(PreviewNotice::warning(
            NoticeKind::PageLimit,
            format!(
                "page range {first}-{last} has {span} pages, more than the limit of {}; \
                 previewing {first}-{capped}",
                options.max_pages
            ),
        ));
        last = capped;
    }
    Some((first, last))
}

/// The DPI for `page`: the requested one, lowered so that the long edge stays
/// within the pixel limit. `None` if even 1 DPI would exceed it.
pub(crate) fn effective_dpi(options: &PreviewOptions, page: &PageInfo) -> Option<u32> {
    let max = f64::from(options.max_long_edge_px) * 72.0 / page.long_edge_pt();
    if max < 1.0 {
        return None;
    }
    // `max` is finite and >= 1; clamp before converting.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max = max.min(f64::from(u32::MAX)).floor() as u32;
    Some(options.dpi.min(max))
}

/// Creates `<root>/<subdir>` component by component, refusing anything that
/// is not a real directory (for example a symlink placed in the output root
/// by the engine).
fn prepare_dir(root: &Path, subdir: &WorkspacePath) -> io::Result<PathBuf> {
    let mut dir = root.to_path_buf();
    for part in subdir.as_str().split('/') {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a file or symlink with that name exists",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(e) => return Err(e),
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use texrun_core::Severity;

    use super::*;
    use crate::options::PageRange;

    type Selected = (Option<(u32, u32)>, Vec<(Severity, NoticeKind)>);

    fn select(count: u32, options: &PreviewOptions) -> Selected {
        let mut notices = Vec::new();
        let sel = select_pages(count, options, &mut notices);
        (sel, notices.iter().map(|n| (n.severity, n.kind)).collect())
    }

    fn range(s: &str) -> PreviewOptions {
        PreviewOptions::default().with_pages(s.parse::<PageRange>().unwrap())
    }

    #[test]
    fn default_selection_is_the_first_20_pages() {
        let o = PreviewOptions::default();
        assert_eq!(select(3, &o), (Some((1, 3)), vec![]));
        assert_eq!(select(20, &o), (Some((1, 20)), vec![]));
        assert_eq!(
            select(21, &o),
            (Some((1, 20)), vec![(Severity::Info, NoticeKind::PageLimit)])
        );
        // The default limit never exceeds the hard limit.
        let o = PreviewOptions::default().with_max_pages(5);
        assert_eq!(select(50, &o).0, Some((1, 5)));
    }

    #[test]
    fn explicit_ranges_are_clamped_and_capped() {
        assert_eq!(select(10, &range("3-5")), (Some((3, 5)), vec![]));
        assert_eq!(select(10, &range("4-")), (Some((4, 10)), vec![]));
        assert_eq!(select(10, &range("7")), (Some((7, 7)), vec![]));
        assert_eq!(
            select(10, &range("8-12")),
            (
                Some((8, 10)),
                vec![(Severity::Info, NoticeKind::PageRangeClamped)]
            )
        );
        assert_eq!(
            select(10, &range("11-")),
            (
                None,
                vec![(Severity::Warning, NoticeKind::PageRangeOutOfBounds)]
            )
        );
        // An explicit range may exceed the default 20 but not the hard 200.
        assert_eq!(select(1000, &range("1-150")), (Some((1, 150)), vec![]));
        assert_eq!(
            select(1000, &range("101-")),
            (
                Some((101, 300)),
                vec![(Severity::Warning, NoticeKind::PageLimit)]
            )
        );
        assert_eq!(
            select(u32::MAX, &range("1-")).0,
            Some((1, 200)),
            "no overflow"
        );
    }

    #[test]
    fn dpi_is_lowered_for_large_pages() {
        let o = PreviewOptions::default();
        let a4 = PageInfo::new(1, 595.0, 842.0, 0);
        assert_eq!(effective_dpi(&o, &a4), Some(144));
        // A0 (2384 x 3370 pt) at 144 DPI would be 6740 px long.
        let a0 = PageInfo::new(1, 2384.0, 3370.0, 0);
        let dpi = effective_dpi(&o, &a0).unwrap();
        assert_eq!(dpi, 87);
        assert!(3370.0 * f64::from(dpi) / 72.0 <= 4096.0);
        // Absurdly large pages are not rendered at all.
        let huge = PageInfo::new(1, 1.0e6, 1.0e6, 0);
        assert_eq!(effective_dpi(&o, &huge), None);
        // Tiny pages keep the requested DPI.
        let tiny = PageInfo::new(1, 0.001, 0.001, 0);
        assert_eq!(effective_dpi(&o, &tiny), Some(144));
    }

    #[cfg(unix)]
    #[test]
    fn prepare_dir_refuses_symlinks_and_files() {
        let root = tempfile::tempdir().unwrap();
        let sub = WorkspacePath::new("a/b").unwrap();
        let dir = prepare_dir(root.path(), &sub).unwrap();
        assert!(dir.is_dir());
        assert!(prepare_dir(root.path(), &sub).is_ok(), "idempotent");

        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let err = prepare_dir(root.path(), &WorkspacePath::new("link/x").unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(!outside.path().join("x").exists());

        fs::write(root.path().join("file"), "").unwrap();
        assert!(prepare_dir(root.path(), &WorkspacePath::new("file").unwrap()).is_err());
    }
}
