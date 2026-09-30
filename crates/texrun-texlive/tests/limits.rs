//! CPU time, memory and process limits of the engine against a real TeX
//! Live + latexmk (docs/security.md §3.10): a compile that exceeds one is
//! stopped and reported as failed with a `resource_limit` diagnostic, and
//! ordinary documents stay well within the defaults.
//!
//! The cgroup tests need a delegated cgroup; see `cgroups()`.
//! See `tests/common/mod.rs` for how the TeX Live tests are enabled.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::time::Duration;

use common::{Compile, assert_outcome, describe, live_group_members, require_texlive};
use tempfile::TempDir;
use texrun_core::{CompileOutcome, DiagnosticKind, EngineError};
use texrun_texlive::{Cgroups, LatexmkConfig, LatexmkEngine, Limits};

/// A project with the given files in a temporary directory.
fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        fs::write(dir.path().join(name), content).unwrap();
    }
    dir
}

fn engine(limits: Limits) -> LatexmkEngine {
    LatexmkEngine::new(LatexmkConfig::default().with_limits(limits))
}

fn limit_messages(run: &texrun_texlive::LatexmkRun) -> Vec<&str> {
    run.result
        .errors()
        .filter(|d| d.kind == DiagnosticKind::ResourceLimit)
        .map(|d| d.message.as_str())
        .collect()
}

/// A document that keeps TeX busy is stopped by the CPU time limit, well
/// before the timeout. Where rlimits cannot be set without an exec gate
/// (macOS) the timeout stops it instead.
#[test]
fn the_cpu_time_limit_stops_an_endless_loop() {
    require_texlive!();
    let limits = Limits::default().with_max_cpu_time(Some(Duration::from_secs(2)));
    let (run, _ws) = Compile::fixture("timeout", "main.tex")
        .timeout(Duration::from_secs(if cfg!(target_os = "linux") {
            60
        } else {
            5
        }))
        .engine(engine(limits))
        .run();
    if cfg!(target_os = "linux") {
        assert_outcome(&run, CompileOutcome::Failed);
        let messages = limit_messages(&run);
        assert!(
            messages.iter().any(|m| m.contains("2 s of CPU time")),
            "{}",
            describe(&run)
        );
        assert!(run.result.elapsed < Duration::from_secs(30));
    } else {
        assert_outcome(&run, CompileOutcome::TimedOut);
    }
    assert!(live_group_members(run.pid).is_empty());
}

/// An engine that cannot get the memory it needs fails, and the result
/// says why (Linux: `RLIMIT_AS`; pdflatex needs about 100 MiB even for a
/// small document).
#[cfg(target_os = "linux")]
#[test]
fn the_address_space_limit_stops_the_engine() {
    require_texlive!();
    let limits = Limits::default().with_max_address_space(48 << 20);
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(engine(limits))
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert!(
        limit_messages(&run)
            .iter()
            .any(|m| m.contains("ran out of memory")),
        "{}",
        describe(&run)
    );
    assert!(run.result.resource_limits.as_ref().unwrap().rlimits);
}

/// A long document (200+ pages with a table of contents, cross references,
/// a bibliography and an index, several latexmk passes) compiles within the
/// default limits.
#[test]
fn a_long_document_stays_within_the_default_limits() {
    require_texlive!();
    let mut tex = String::from(
        "\\documentclass{report}\n\\usepackage{makeidx}\n\\makeindex\n\
         \\begin{document}\n\\tableofcontents\n",
    );
    for chapter in 1..=50 {
        let _ = writeln!(tex, "\\chapter{{Chapter {chapter}}}\\label{{ch:{chapter}}}");
        for section in 1..=4 {
            let _ = writeln!(
                tex,
                "\\section{{Section {section}}}\\label{{s:{chapter}.{section}}}"
            );
            for paragraph in 1..=6 {
                let _ = writeln!(
                    tex,
                    "Paragraph {paragraph} refers to chapter~\\ref{{ch:{chapter}}} on \
                     page~\\pageref{{s:{chapter}.{section}}} and cites~\\cite{{k{}}}.\
                     \\index{{term {paragraph}}} Lorem ipsum dolor sit amet, consectetur \
                     adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore \
                     magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco \
                     laboris nisi ut aliquip ex ea commodo consequat.\n",
                    (chapter + paragraph) % 20
                );
            }
        }
    }
    tex.push_str(
        "\\bibliographystyle{plain}\n\\bibliography{refs}\n\\printindex\n\\end{document}\n",
    );
    let mut bib = String::new();
    for key in 0..20 {
        let _ = writeln!(
            bib,
            "@book{{k{key}, author={{Author {key}}}, title={{Title {key}}}, \
             publisher={{P}}, year={{2000}}}}"
        );
    }
    let project = project(&[("main.tex", &tex), ("refs.bib", &bib)]);
    let (run, _ws) = Compile::new(project.path(), "main.tex").run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    assert!(limit_messages(&run).is_empty(), "{}", describe(&run));
    let limits = run.result.resource_limits.as_ref().unwrap();
    assert_eq!(limits.rlimits, cfg!(target_os = "linux"), "{limits:?}");
    assert!(!limits.cgroup, "no cgroup was asked for");
}

#[test]
fn a_required_cgroup_that_cannot_be_used_is_unsupported() {
    require_texlive!();
    let config = LatexmkConfig::default()
        .with_cgroups(Cgroups::unavailable("no delegated cgroup").with_required(true));
    let ws = Compile::fixture("minimal", "main.tex").workspace();
    let err = LatexmkEngine::new(config)
        .run(&ws.context(), ws.request())
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Unsupported(m) if m.contains("no delegated cgroup")),
        "{err:?}"
    );
    assert!(!ws.output_dir().join("main.pdf").exists());
}

#[test]
fn an_optional_cgroup_that_cannot_be_used_is_recorded() {
    require_texlive!();
    let config = LatexmkConfig::default().with_cgroups(Cgroups::unavailable("no delegated cgroup"));
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(LatexmkEngine::new(config))
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let limits = run.result.resource_limits.as_ref().unwrap();
    assert!(!limits.cgroup);
    assert!(
        limits
            .notes
            .iter()
            .any(|n| n == "cgroup: no delegated cgroup"),
        "{limits:?}"
    );
}

/// The delegated cgroup of this test process, or `None` (the test is
/// skipped) unless `TEXRUN_REQUIRE_CGROUP=1`.
#[cfg(target_os = "linux")]
fn cgroups() -> Option<Cgroups> {
    use std::sync::OnceLock;
    static CGROUPS: OnceLock<Cgroups> = OnceLock::new();
    let cgroups = CGROUPS.get_or_init(Cgroups::detect);
    match cgroups.check() {
        Ok(()) => Some(cgroups.clone()),
        Err(reason) if std::env::var_os("TEXRUN_REQUIRE_CGROUP").is_some_and(|v| v == "1") => {
            panic!("TEXRUN_REQUIRE_CGROUP=1, but no cgroup can be used: {reason}")
        }
        Err(reason) => {
            eprintln!("skipped: no delegated cgroup ({reason})");
            None
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cgroup_default_limits_allow_an_ordinary_compile() {
    require_texlive!();
    let Some(cgroups) = cgroups() else { return };
    let config = LatexmkConfig::default().with_cgroups(cgroups.with_required(true));
    let (run, _ws) = Compile::fixture("references", "main.tex")
        .engine(LatexmkEngine::new(config))
        .run();
    assert_outcome(&run, CompileOutcome::Succeeded);
    let limits = run.result.resource_limits.as_ref().unwrap();
    assert!(limits.cgroup && limits.rlimits, "{limits:?}");
}

/// Memory beyond the cgroup's `memory.max` gets the compile OOM-killed.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_memory_limit_stops_the_engine() {
    require_texlive!();
    let Some(cgroups) = cgroups() else { return };
    let limits = Limits::default().with_max_memory_bytes(24 << 20);
    let config = LatexmkConfig::default()
        .with_limits(limits)
        .with_cgroups(cgroups.with_required(true));
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .engine(LatexmkEngine::new(config))
        .run();
    assert_outcome(&run, CompileOutcome::Failed);
    assert!(
        limit_messages(&run).iter().any(|m| m.contains("memory")),
        "{}",
        describe(&run)
    );
    assert!(live_group_members(run.pid).is_empty());
}

/// latexmk cannot start pdflatex beyond the cgroup's `pids.max`. perl
/// retries a refused `fork`, so the timeout ends the compile; the result
/// says why it did not finish.
#[cfg(target_os = "linux")]
#[test]
fn cgroup_process_limit_stops_the_engine() {
    require_texlive!();
    let Some(cgroups) = cgroups() else { return };
    let limits = Limits::default().with_max_processes(1);
    let config = LatexmkConfig::default()
        .with_limits(limits)
        .with_cgroups(cgroups.with_required(true));
    let (run, _ws) = Compile::fixture("minimal", "main.tex")
        .timeout(Duration::from_secs(8))
        .engine(LatexmkEngine::new(config))
        .run();
    assert!(
        matches!(
            run.result.outcome,
            CompileOutcome::TimedOut | CompileOutcome::Failed
        ),
        "{}",
        describe(&run)
    );
    assert!(
        limit_messages(&run)
            .iter()
            .any(|m| m.contains("more than 1 processes")),
        "{}",
        describe(&run)
    );
}
