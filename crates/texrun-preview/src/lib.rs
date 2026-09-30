//! Page previews and PDF metadata for texrun.
//!
//! After a successful compile, the pages of the PDF are rendered to PNG files
//! so that an AI agent can look at the final layout. Rendering is done by
//! external tools, detected on `PATH` ([`Toolset`]):
//!
//! - **`MuPDF`** (`mutool`) is preferred: it rendered an A4 text page about
//!   six times faster than `pdftoppm` in the development container, which
//!   matters for the 30 s budget of up to 200 pages;
//! - **Poppler** (`pdfinfo` + `pdftoppm`) is the fallback, as it is more
//!   often preinstalled.
//!
//! Either can be chosen explicitly ([`BackendChoice`]), e.g. where
//! AGPL-licensed software (`MuPDF`) is not wanted at all. Both are only ever
//! started as separate processes of an installed binary; texrun neither
//! links nor ships them.
//!
//! ```no_run
//! use std::path::Path;
//! use texrun_preview::{PreviewOptions, Previewer};
//!
//! let report = Previewer::detect()
//!     .render(Path::new("out/main.pdf"), Path::new("out"), &PreviewOptions::default())
//!     .expect("options are valid");
//! for page in &report.pages {
//!     println!("{} ({} x {} px)", page.artifact.path.as_str(), page.width_px, page.height_px);
//! }
//! for notice in &report.notices {
//!     eprintln!("preview: {}", notice.message);
//! }
//! ```
//!
//! # Degraded behavior
//!
//! Previews are an add-on to a compile, never a reason for it to fail. The
//! only error is invalid [`PreviewOptions`]; everything else (no tool
//! installed, an unreadable PDF, a tool failure on one page, a limit, the
//! timeout, cancellation) yields a [`PreviewReport`] with a
//! [`PreviewStatus`] and [`PreviewNotice`]s, alongside whatever pages were
//! rendered. The caller keeps the compile outcome and adds the report
//! ([`PreviewReport::attach_to`] appends the images to
//! [`CompileResult::artifacts`](texrun_core::CompileResult::artifacts)).
//!
//! # Limits and untrusted input
//!
//! The PDF comes from an untrusted document and is treated as untrusted input
//! (docs/security.md). Defaults ([`PreviewOptions::default`]):
//!
//! | limit | default |
//! | --- | --- |
//! | pages without a range | first [`DEFAULT_PAGE_LIMIT`] (20) |
//! | pages with a range | at most [`MAX_PAGE_LIMIT`] (200) |
//! | total image size | [`DEFAULT_MAX_TOTAL_BYTES`] (128 MiB); rendering stops before the page that would exceed it |
//! | long edge of one image | [`DEFAULT_MAX_LONG_EDGE_PX`] (4096 px); see below |
//! | wall clock, all pages | [`DEFAULT_TIMEOUT`] (30 s) |
//!
//! The long edge limit is enforced in layers, so that it does not depend on
//! parsing the (untrusted) page size correctly: the DPI is lowered for large
//! pages (including the `UserUnit` scale that `mutool` honors), `mutool draw`
//! also gets the limit as a bounding box (`-w`/`-h`), and every image is
//! checked after rendering and discarded (`render_failed`) if it is larger.
//!
//! Tools are run by the shared supervisor of `texrun-process`: started by
//! absolute path with an argv array (no shell), with a
//! cleared environment (`PATH` with only its absolute entries, `LC_ALL=C` and
//! a private empty `HOME`), in their own process group that is killed with
//! `SIGKILL` on timeout, cancellation or when an image outgrows the remaining
//! size budget. `RLIMIT_FSIZE`, `RLIMIT_CORE` and (Linux only) `RLIMIT_AS`
//! (2 GiB) are set on each tool before it starts, by an exec gate
//! ([`Previewer::with_exec_gate`]; the texrun CLI hosts one). Without a gate
//! they are set with `prlimit(2)` right after the spawn (best effort, Linux
//! only). A tool's memory use is not capped on macOS. Tool output is parsed defensively. Images are written to a
//! private scratch directory (created with `mkdirat` below the output root
//! and held open; the tools run in its held `work` directory), checked
//! (regular file, PNG header, size and
//! pixel budget) and moved into place with `renameat` between directory
//! descriptors; the preview directory is created and opened with
//! `mkdirat` / `openat(O_NOFOLLOW)`, so a symlink swapped into the output
//! root cannot redirect the images.
//!
//! Unix only (Linux, macOS), like the workspace crate.

#[cfg(not(unix))]
compile_error!("texrun-preview supports Unix hosts only");

mod backend;
mod error;
mod fsops;
mod options;
mod png;
mod process;
mod render;
mod report;
mod tools;

pub use error::PreviewError;
pub use options::{
    BackendChoice, DEFAULT_DPI, DEFAULT_MAX_LONG_EDGE_PX, DEFAULT_MAX_TOTAL_BYTES,
    DEFAULT_PAGE_LIMIT, DEFAULT_TIMEOUT, MAX_DPI, MAX_PAGE_LIMIT, PageRange, PageRangeError,
    PreviewOptions,
};
pub use render::Previewer;
pub use report::{
    BackendKind, ImageFormat, NoticeKind, PageInfo, PagePreview, PdfInfo, PreviewNotice,
    PreviewReport, PreviewStatus,
};
/// The exec gate for [`Previewer::with_exec_gate`] (from `texrun-process`).
pub use texrun_process::ExecGate;
pub use tools::{MUTOOL, PDFINFO, PDFTOPPM, Toolset};
