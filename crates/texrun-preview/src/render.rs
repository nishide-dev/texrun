//! The preview run: metadata, page selection and rendering.

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use texrun_core::{Artifact, ArtifactKind, WorkspacePath};
use texrun_process::{Cgroups, ExecGate, Resource, Rlimits};
use texrun_sandbox::{SandboxError, Session};

use crate::backend::Invocation;
use crate::error::PreviewError;
use crate::fsops;
use crate::options::PreviewOptions;
use crate::png;
use crate::process::{self, Limits, RunEnd, RunOutput, ToolEnv};
use crate::report::{
    NoticeKind, PageInfo, PagePreview, PdfInfo, PreviewNotice, PreviewReport, PreviewStatus,
};
use crate::sandbox::{self, PreviewContainer};
use crate::tools::{Backend, Toolset};

/// The notice for tools started without the exec gate because of `reason`.
fn fallback_notice(reason: &str) -> PreviewNotice {
    PreviewNotice::warning(
        NoticeKind::ResourceLimits,
        format!(
            "the resource limits of the preview tools are set only after they start \
             (best effort), because the exec gate cannot be used ({reason})"
        ),
    )
}

/// The notice for tools run without their cgroup because of `reason`.
fn cgroup_notice(reason: &str) -> PreviewNotice {
    PreviewNotice::warning(
        NoticeKind::ResourceLimits,
        format!(
            "the preview tools run without a cgroup of their own (only their rlimits \
             apply), because none can be used ({reason})"
        ),
    )
}

/// Characters of tool stderr kept in a notice.
const DETAIL_CHARS: usize = 2000;
/// File name the tool renders page `page` to, inside the private working
/// directory. A new name for every page: in a container, the runtime's file
/// sharing (on macOS, with its VM) may still cache a name that texrun has
/// moved away on the host, so a name is never written twice.
fn render_name(page: u32) -> String {
    format!("render-{page:03}.png")
}
/// Pixels an image may exceed `max_long_edge_px` by (rounding in the tools).
const PIXEL_SLACK: u32 = 2;

/// Generates page previews and reads PDF metadata with the external tools of
/// a [`Toolset`].
#[derive(Debug, Clone)]
pub struct Previewer {
    tools: Toolset,
    gate: Option<ExecGate>,
    cgroups: Option<Cgroups>,
    container: Option<PreviewContainer>,
}

impl Previewer {
    /// Uses the tools of `tools`.
    pub fn new(tools: Toolset) -> Self {
        Self {
            tools,
            gate: None,
            cgroups: None,
            container: None,
        }
    }

    /// Runs the tools of the image of `container` in a container instead of
    /// the host's tools (docs/security.md §4 "preview", #46): one container
    /// per preview run, with no network, nothing of the host but a copy of
    /// the PDF (read-only) and the tools' scratch directories (writable), and
    /// the limits of §3.10 (see [`PreviewContainer`]). The images are checked
    /// and stored through descriptors as on the host.
    ///
    /// The tools are looked up in the image (which of `mutool`, `pdfinfo`
    /// and `pdftoppm` exist in `/usr/bin`) at the start of each run;
    /// [`Previewer::toolset`] is empty. The exec gate and cgroups
    /// ([`Previewer::with_exec_gate`], [`Previewer::with_cgroups`]) are not
    /// used: the runtime applies the limits. If the container cannot be
    /// started, or the runtime did not apply one of its restrictions (fail
    /// closed), no tool runs (on the host neither): the status is
    /// [`PreviewStatus::Skipped`] with a notice.
    pub fn in_container(container: PreviewContainer) -> Self {
        Self {
            tools: Toolset::none(),
            gate: None,
            cgroups: None,
            container: Some(container),
        }
    }

    /// Starts the tools through `gate`, which sets their resource limits
    /// before they start (docs/security.md §3.2; see
    /// [`StartMode::ExecGate`](texrun_process::StartMode::ExecGate)).
    ///
    /// Without a gate the limits are set with `prlimit(2)` right after each
    /// tool was spawned (Linux only, best effort). If `gate` cannot be used
    /// ([`ExecGate::check`]), the report says so with a
    /// [`NoticeKind::ResourceLimits`] warning, and
    ///
    /// - with [`ExecGate::with_required`], no tool is run: the status is
    ///   [`PreviewStatus::Skipped`] (the texrun CLI does this);
    /// - otherwise the tools run with the best-effort limits above.
    #[must_use]
    pub fn with_exec_gate(mut self, gate: ExecGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Runs every tool in a cgroup of its own, with limits on its memory,
    /// processes and CPU use (Linux, docs/security.md §3.10; see
    /// [`Cgroups`]). If `cgroups` cannot be used, the report says so with a
    /// [`NoticeKind::ResourceLimits`] warning, and with
    /// [`Cgroups::with_required`] no tool is run (as for a required exec
    /// gate); otherwise the tools run with their rlimits only.
    #[must_use]
    pub fn with_cgroups(mut self, cgroups: Cgroups) -> Self {
        self.cgroups = Some(cgroups);
        self
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
    /// without rendering anything. On success the status is
    /// [`PreviewStatus::Inspected`] and [`PreviewReport::pdf`] is set; if the
    /// metadata cannot be read it is [`PreviewStatus::Skipped`] with notices.
    /// A selection outside the document (e.g. a range after the last page)
    /// still counts as inspected, with the page count and no page sizes.
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
        if let Some(container) = &self.container {
            return Ok(run_in_container(container, pdf, output_root, options));
        }
        let mut report = PreviewReport::empty();
        let backend = match self.tools.select(options.backend) {
            Ok(backend) => backend,
            Err(msg) => {
                report.push(PreviewNotice::warning(NoticeKind::ToolUnavailable, msg));
                return Ok(report);
            }
        };
        report.backend = Some(backend.kind());
        if let Some(gate) = &self.gate
            && let Err(reason) = gate.check()
        {
            if gate.is_required() {
                report.push(PreviewNotice::warning(
                    NoticeKind::ResourceLimits,
                    format!(
                        "previews were not rendered: the preview tools must start with \
                         their resource limits in place, but the exec gate cannot be \
                         used ({reason})"
                    ),
                ));
                return Ok(report);
            }
            report.push(fallback_notice(&reason));
        }
        if let Some(cgroups) = &self.cgroups
            && let Err(reason) = cgroups.check()
        {
            if cgroups.is_required() {
                report.push(PreviewNotice::warning(
                    NoticeKind::ResourceLimits,
                    format!(
                        "previews were not rendered: the preview tools must run in a cgroup \
                         of their own, but none can be used ({reason})"
                    ),
                ));
                return Ok(report);
            }
            report.push(cgroup_notice(&reason));
        }
        let mut run = Run {
            backend,
            gate: self.gate.as_ref(),
            cgroups: self.cgroups.as_ref().filter(|c| c.check().is_ok()),
            sandbox: None,
            options,
            deadline: Instant::now() + options.timeout,
            report,
            selected: 0,
            inspected: false,
        };
        run.execute(pdf, output_root, self.tools.search_path());
        Ok(run.finish())
    }
}

struct Run<'a> {
    backend: Backend<'a>,
    gate: Option<&'a ExecGate>,
    cgroups: Option<&'a Cgroups>,
    /// The container the tools run in, if any.
    sandbox: Option<&'a Session<'a>>,
    options: &'a PreviewOptions,
    deadline: Instant,
    report: PreviewReport,
    selected: u32,
    /// Metadata-only run that read the metadata successfully.
    inspected: bool,
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
        let Ok(root_fd) = self.open_root(output_root) else {
            return;
        };
        // Private scratch space: `HOME` for the tools and their working
        // directory. For rendering it lives in the output root so that
        // finished images can be renamed into place. It is created and
        // opened through descriptors (`mkdirat` / `openat(O_NOFOLLOW)`), and
        // the tools run in, and write to, the held `work` directory.
        let scratch = if let (Some(root), Some(fd)) = (output_root, &root_fd) {
            // The same (held) output root the images are moved into.
            fd.try_clone()
                .and_then(|fd| fsops::ScratchDir::create(fd, root, ".texrun-preview-"))
        } else {
            let tmp = std::env::temp_dir();
            fsops::open_dir(&tmp)
                .and_then(|fd| fsops::ScratchDir::create(fd, &tmp, "texrun-preview-"))
        };
        let scratch = match scratch.and_then(|dir| {
            dir.subdir("home")?;
            let work = dir.subdir("work")?;
            Ok((dir, work))
        }) {
            Ok(scratch) => scratch,
            Err(e) => {
                self.notice(PreviewNotice::warning(
                    NoticeKind::OutputError,
                    format!("cannot create a scratch directory for the preview tools: {e}"),
                ));
                return;
            }
        };
        let (scratch, work) = scratch;
        let env = ToolEnv {
            path: search_path.map(std::ffi::OsStr::to_os_string),
            home: scratch.path().join("home"),
            work,
            work_path: scratch.path().join("work"),
            guest_work: None,
        };
        self.process(&pdf, root_fd, &env);
    }

    /// Opens the output root, if any; an error (with a notice) if that
    /// fails.
    fn open_root(&mut self, output_root: Option<&Path>) -> Result<Option<OwnedFd>, ()> {
        output_root.map(fsops::open_dir).transpose().map_err(|e| {
            self.notice(PreviewNotice::warning(
                NoticeKind::OutputError,
                format!("cannot open the output directory: {e}"),
            ));
        })
    }

    /// Reads the metadata of `pdf` (as the tools see it) and renders the
    /// selected pages into `root_fd`, if any.
    fn process(&mut self, pdf: &Path, root_fd: Option<OwnedFd>, env: &ToolEnv) {
        let Some(count) = self.page_count(pdf, env) else {
            return;
        };
        self.report.pdf = Some(PdfInfo {
            page_count: count,
            pages: Vec::new(),
        });
        let Some((first, last)) = select_pages(count, self.options, &mut self.report.notices)
        else {
            self.inspected = root_fd.is_none();
            return;
        };
        let Some(pages) = self.page_sizes(pdf, env, first, last) else {
            return;
        };
        if let Some(info) = self.report.pdf.as_mut() {
            info.pages.clone_from(&pages);
        }
        match root_fd {
            Some(root_fd) => {
                self.selected = last - first + 1;
                self.render_pages(pdf, &root_fd, env, first, last, &pages);
            }
            None => self.inspected = true,
        }
    }

    fn finish(mut self) -> PreviewReport {
        let rendered = u32::try_from(self.report.pages.len()).unwrap_or(u32::MAX);
        self.report.status = if self.inspected {
            PreviewStatus::Inspected
        } else if rendered == 0 {
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
        &mut self,
        inv: &Invocation<'_>,
        env: &ToolEnv,
        watch: Option<(&str, u64)>,
    ) -> RunOutput {
        let out = process::run(
            inv.program,
            &inv.args,
            env,
            watch.is_none(),
            Limits {
                gate: self.gate,
                cgroups: self.cgroups,
                sandbox: self.sandbox,
                cpu_seconds: process::cpu_seconds(self.options.timeout),
                deadline: self.deadline,
                cancel: &self.options.cancel,
                watch,
            },
        );
        // Reported once per preview run (e.g. the gate disappeared since the
        // check up front).
        let degraded = [
            out.gate_fallback.as_deref().map(fallback_notice),
            out.cgroup_unavailable.as_deref().map(cgroup_notice),
        ];
        for notice in degraded.into_iter().flatten() {
            if !self.report.notices.contains(&notice) {
                self.report.push(notice);
            }
        }
        out
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
            RunEnd::LimitExceeded(what) => PreviewNotice::warning(
                NoticeKind::LimitExceeded,
                format!("preview generation stopped: {} {what}", program_name(inv)),
            ),
            RunEnd::LimitsUnavailable(reason) => PreviewNotice::warning(
                NoticeKind::ResourceLimits,
                format!(
                    "{} was not run: its resource limits cannot be put in place ({reason})",
                    program_name(inv)
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
        root_fd: &OwnedFd,
        env: &ToolEnv,
        first: u32,
        last: u32,
        pages: &[PageInfo],
    ) {
        let dir = match fsops::ensure_subdir(root_fd, &self.options.output_subdir) {
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

    /// The DPI for `info` (see [`effective_dpi`]), with a notice when it is
    /// lowered, or `None` (and a warning) if the page cannot be rendered.
    fn page_dpi(&mut self, info: &PageInfo) -> Option<u32> {
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
            return None;
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
        Some(dpi)
    }

    /// Renders one page into the preview directory `dir`. `Break` stops the
    /// whole run (timeout, cancellation, size limit, output error); a failure
    /// of this page only is reported and returns `Continue`.
    fn render_page(
        &mut self,
        pdf: &Path,
        env: &ToolEnv,
        dir: &OwnedFd,
        info: &PageInfo,
        total: &mut u64,
    ) -> ControlFlow<()> {
        let page = info.page;
        let max_px = self.options.max_long_edge_px;
        let Some(dpi) = self.page_dpi(info) else {
            return ControlFlow::Continue(());
        };
        let budget = self.options.max_total_bytes.saturating_sub(*total);
        let render_name = render_name(page);
        let render_name = render_name.as_str();
        fsops::remove(&env.work, render_name);
        let inv = self.backend.render(pdf, page, dpi, max_px, render_name);
        let out = self.invoke(&inv, env, Some((render_name, budget)));
        if self.stopped(&inv, &out, Some(page)) {
            fsops::remove(&env.work, render_name);
            return ControlFlow::Break(());
        }
        let size = fsops::regular_file_len(&env.work, render_name);
        // Checked before the exit status: on Linux a tool that reaches
        // `RLIMIT_FSIZE` is killed by `SIGXFSZ`.
        if size.is_some_and(|size| size > budget) {
            fsops::remove(&env.work, render_name);
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
        if !out.succeeded() {
            fsops::remove(&env.work, render_name);
            self.render_failed(page, &format!("{} failed", program_name(&inv)), &out);
            return ControlFlow::Continue(());
        }
        let Some(size) = size else {
            self.render_failed(page, "no image was written", &out);
            return ControlFlow::Continue(());
        };
        let Some((width_px, height_px)) =
            fsops::read_header::<24>(&env.work, render_name).and_then(|h| png::dimensions(&h))
        else {
            fsops::remove(&env.work, render_name);
            self.render_failed(page, "the output is not a PNG image", &out);
            return ControlFlow::Continue(());
        };
        // Last line of defense for the pixel limit, whatever the tool made of
        // the page size (allowing for rounding).
        if width_px.max(height_px) > max_px.saturating_add(PIXEL_SLACK) {
            fsops::remove(&env.work, render_name);
            self.render_failed(
                page,
                &format!(
                    "the image ({width_px} x {height_px} px) exceeds the limit of {max_px} px"
                ),
                &out,
            );
            return ControlFlow::Continue(());
        }
        let name = WorkspacePath::new(&format!("page-{page:03}.png"))
            .expect("generated file name is a valid path");
        if let Err(e) = fsops::rename(&env.work, render_name, dir, name.as_str()) {
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

/// [`Previewer::render`] / [`Previewer::inspect`] with the tools in a
/// container (see [`Previewer::in_container`]).
#[allow(
    clippy::too_many_lines,
    reason = "one sequence of steps, each ending the run with its own notice"
)]
fn run_in_container(
    container: &PreviewContainer,
    pdf: &Path,
    output_root: Option<&Path>,
    options: &PreviewOptions,
) -> PreviewReport {
    let mut run = Run {
        // Replaced once the tools of the image are known.
        backend: Backend::Mupdf {
            mutool: Path::new(""),
        },
        gate: None,
        cgroups: None,
        sandbox: None,
        options,
        deadline: Instant::now() + options.timeout,
        report: PreviewReport::empty(),
        selected: 0,
        inspected: false,
    };
    // Known before the image is: an explicit choice (Auto depends on the
    // tools of the image).
    run.report.backend = match options.backend {
        crate::BackendChoice::Poppler => Some(crate::BackendKind::Poppler),
        crate::BackendChoice::Mupdf => Some(crate::BackendKind::Mupdf),
        _ => None,
    };
    let Some(pdf) = run.check_pdf(pdf) else {
        return run.finish();
    };
    let Ok(root_fd) = run.open_root(output_root) else {
        return run.finish();
    };
    // Neither a container nor a scratch directory for a run that is over.
    let over = if options.cancel.is_cancelled() {
        Some(RunEnd::Cancelled)
    } else if Instant::now() >= run.deadline {
        Some(RunEnd::TimedOut)
    } else {
        None
    };
    if let Some(end) = over {
        let inv = Invocation {
            program: Path::new("/usr/bin/ls"),
            args: Vec::new(),
        };
        run.stopped(&inv, &RunOutput::empty(end), None);
        return run.finish();
    }
    // The scratch directory is private and outside the output root (which
    // left-over processes of the compile could have written to), because
    // the runtime resolves the mount sources by their path. `in` holds a
    // copy of the PDF, readable by everyone (the container user is another
    // uid when texrun runs as root).
    let (scratch, work) = match container_scratch(container, &pdf) {
        Ok(scratch) => scratch,
        Err(e) => {
            run.notice(PreviewNotice::warning(
                NoticeKind::OutputError,
                format!("cannot prepare a scratch directory for the preview container: {e}"),
            ));
            return run.finish();
        }
    };
    let spec = container.spec(
        &scratch.path().join("in"),
        &scratch.path().join("work"),
        &scratch.path().join("home"),
    );
    let lifetime = sandbox::lifetime(options.timeout);
    let ulimits = container_ulimits(options);
    let session = match Session::start(container.runtime(), spec, lifetime, ulimits) {
        Ok(session) => session,
        Err(SandboxError::Refused(reason)) => {
            run.notice(PreviewNotice::warning(
                NoticeKind::ResourceLimits,
                format!("previews were not rendered: {reason}"),
            ));
            return run.finish();
        }
        Err(e) => {
            run.notice(PreviewNotice::warning(
                NoticeKind::ToolUnavailable,
                format!("cannot start the preview container: {e}"),
            ));
            return run.finish();
        }
    };
    for warning in session.warnings() {
        run.notice(PreviewNotice::info(
            NoticeKind::ResourceLimits,
            format!("container: {warning}"),
        ));
    }
    let env = ToolEnv {
        path: Some(sandbox::GUEST_PATH.into()),
        home: PathBuf::from(sandbox::GUEST_HOME),
        work,
        work_path: scratch.path().join("work"),
        guest_work: Some(PathBuf::from(sandbox::GUEST_WORK)),
    };

    // Which tools the image has: those of `guest_tools` that `ls` lists.
    let candidates = sandbox::guest_tools();
    let ls = Invocation {
        program: Path::new("/usr/bin/ls"),
        args: ["-1", "--"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .chain(candidates.iter().map(|t| t.as_os_str().to_owned()))
            .collect(),
    };
    run.sandbox = Some(&session);
    let out = run.invoke(&ls, &env, None);
    if run.stopped(&ls, &out, None) {
        return run.finish();
    }
    let found = sandbox::listed(&String::from_utf8_lossy(&out.stdout), &candidates);
    let tools = Toolset::at(sandbox::GUEST_PATH, &found);
    let backend = match tools.select(options.backend) {
        Ok(backend) => backend,
        Err(message) => {
            run.notice(PreviewNotice::warning(
                NoticeKind::ToolUnavailable,
                format!("{message} in the container image `{}`", container.image()),
            ));
            return run.finish();
        }
    };
    let mut run = Run { backend, ..run };
    run.report.backend = Some(backend.kind());
    let guest_pdf = Path::new(sandbox::GUEST_INPUT).join(sandbox::PDF_NAME);
    run.process(&guest_pdf, root_fd, &env);
    let report = run.finish();
    // The container goes before its scratch directory.
    drop(session);
    drop(scratch);
    report
}

/// The scratch directory of a preview in a container, with a copy of `pdf`
/// in `in`, and its held `work` directory.
fn container_scratch(
    container: &PreviewContainer,
    pdf: &Path,
) -> io::Result<(fsops::ScratchDir, OwnedFd)> {
    let parent = fs::canonicalize(container.scratch_parent())?;
    let dir = fsops::ScratchDir::create(fsops::open_dir(&parent)?, &parent, "texrun-preview-")?;
    let input = dir.subdir_with_mode("in", rustix::fs::Mode::from_raw_mode(0o755))?;
    fsops::copy_in(pdf, &input, sandbox::PDF_NAME)?;
    dir.subdir("home")?;
    let work = dir.subdir("work")?;
    Ok((dir, work))
}

/// The hard limits of every process in the preview container; each tool
/// gets its own (lower or equal) values with `prlimit`.
fn container_ulimits(options: &PreviewOptions) -> Rlimits {
    let cpu = process::cpu_seconds(options.timeout);
    Rlimits::new()
        .with(
            Resource::FileSize,
            options
                .max_total_bytes
                .saturating_add(1)
                .max(process::MIN_FILE_SIZE_LIMIT),
        )
        .with_soft_hard(
            Resource::Cpu,
            cpu,
            cpu.saturating_add(process::CPU_KILL_GRACE),
        )
        .with(Resource::Core, 0)
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
}
