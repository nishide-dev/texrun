//! Run-time selection of tests that need TeX Live or a preview tool.
//!
//! A minimal copy of `crates/texrun-texlive/tests/common` (same variables and
//! behaviour, see docs/development.md): tests that need TeX Live start with
//! [`require_texlive!`] and are skipped, with one `SKIPPED` line on the real
//! stderr per test binary, when `latexmk` is not on `PATH`. With
//! `TEXRUN_REQUIRE_TEXLIVE=1` (CI integration job, dev container) a missing
//! TeX Live is a failure instead; `TEXRUN_REQUIRE_PREVIEW_TOOLS=1` does the
//! same for the preview tools.

#![allow(dead_code)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use texrun_core::TypesetEngine;
use texrun_preview::Previewer;
use texrun_texlive::LatexmkEngine;

/// Set to `1` to fail (instead of skip) tests that need TeX Live.
pub const REQUIRE_TEXLIVE_ENV: &str = "TEXRUN_REQUIRE_TEXLIVE";

/// Set to `1` to fail (instead of skip) tests that need a preview tool.
pub const REQUIRE_PREVIEW_TOOLS_ENV: &str = "TEXRUN_REQUIRE_PREVIEW_TOOLS";

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

/// Tells that `what` is skipped, once per test binary, on the process's
/// stderr (not captured by libtest).
fn report_skip(what: &'static str, var: &str) {
    static REPORTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut reported = REPORTED.lock().unwrap_or_else(PoisonError::into_inner);
    if reported.contains(&what) {
        return;
    }
    reported.push(what);
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let binary = binary.rsplit_once('-').map_or(binary.as_str(), |(b, _)| b);
    let _ = writeln!(
        std::io::stderr(),
        "texrun {binary}: SKIPPED {what} (set {var}=1 to fail instead)"
    );
}

/// Whether TeX Live (latexmk) is available. Panics if it is not and
/// [`REQUIRE_TEXLIVE_ENV`] is `1`.
pub fn texlive_available() -> bool {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    let probe = PROBE.get_or_init(|| {
        LatexmkEngine::default()
            .probe()
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    match probe {
        Ok(()) => true,
        Err(e) if required(REQUIRE_TEXLIVE_ENV) => {
            panic!("TeX Live is required ({REQUIRE_TEXLIVE_ENV}=1) but not usable: {e}")
        }
        Err(_) => {
            report_skip("all TeX Live tests: latexmk not found", REQUIRE_TEXLIVE_ENV);
            false
        }
    }
}

/// Whether a preview tool (`mutool` or Poppler) is installed. Panics if not
/// and [`REQUIRE_PREVIEW_TOOLS_ENV`] is `1`.
pub fn preview_tools_available() -> bool {
    if !Previewer::detect().toolset().available().is_empty() {
        return true;
    }
    assert!(
        !required(REQUIRE_PREVIEW_TOOLS_ENV),
        "a preview tool (mutool or pdfinfo + pdftoppm) is required \
         ({REQUIRE_PREVIEW_TOOLS_ENV}=1) but none is installed"
    );
    report_skip(
        "preview tests: no preview tool found",
        REQUIRE_PREVIEW_TOOLS_ENV,
    );
    false
}

/// Returns from the calling test unless TeX Live is available.
macro_rules! require_texlive {
    () => {
        if !common::texlive_available() {
            return;
        }
    };
}
pub(crate) use require_texlive;

/// Returns from the calling test unless a preview tool is available.
macro_rules! require_preview_tools {
    () => {
        if !common::preview_tools_available() {
            return;
        }
    };
}
pub(crate) use require_preview_tools;

/// A fixture project of the engine crate:
/// `crates/texrun-texlive/tests/fixtures/<name>`. Compile it with `--output`
/// pointing elsewhere, so nothing is written next to the fixture sources.
pub fn texlive_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/texrun-texlive/tests/fixtures")
        .join(name)
}
