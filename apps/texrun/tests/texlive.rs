//! End-to-end tests of `texrun compile` against a real TeX Live + latexmk.
//!
//! Selected at run time like the engine crate's integration tests
//! (docs/development.md): each test starts with `require_texlive!()` and is
//! skipped when latexmk is missing, unless `TEXRUN_REQUIRE_TEXLIVE=1` (and
//! `TEXRUN_REQUIRE_PREVIEW_TOOLS=1` for previews) makes that a failure:
//!
//! ```text
//! docker compose run --rm -e TEXRUN_REQUIRE_TEXLIVE=1 -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 \
//!     dev cargo test -p texrun
//! ```

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn texrun(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_texrun"));
    cmd.args(args).current_dir(dir);
    cmd
}

fn run_json(dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let out: Output = texrun(dir, args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let doc = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}\n{stderr}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (out.status.code().unwrap(), doc, stderr)
}

/// Arguments to compile the engine crate's `timeout` fixture (a macro that
/// expands forever) with `--output` in `out`, so nothing is written next to
/// the fixture sources.
fn timeout_fixture_args<'a>(out: &'a str, entry: &'a str, timeout: &'a str) -> Vec<&'a str> {
    vec!["compile", "--json", "--timeout", timeout, "-o", out, entry]
}

#[test]
fn texlive_compiles_a_document_with_includes() {
    common::require_texlive!();
    let dir = project(&[
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\input{chapters/intro}\n\\end{document}\n",
        ),
        ("chapters/intro.tex", "Hello from texrun.\n"),
    ]);
    let (code, doc, stderr) = run_json(dir.path(), &["compile", "--json", "main.tex"]);
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "succeeded");
    assert_eq!(doc["texrun_exit_code"], 0);
    assert!(
        doc["engine"]["version"]
            .as_str()
            .unwrap()
            .starts_with("latexmk ")
    );
    let pdf = Path::new(doc["output_dir"].as_str().unwrap()).join("main.pdf");
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    assert!(dir.path().join("texrun-out/main.pdf").is_file());
    // latexmk started through the exec gate (texrun itself), so the
    // rlimits were in place on every platform.
    assert_eq!(doc["resource_limits"]["rlimits"], true, "{doc:#}");

    // Human-readable mode.
    let out = texrun(dir.path(), &["compile", "main.tex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("PDF: texrun-out/main.pdf"), "{text}");
}

#[test]
fn texlive_compiles_a_fixture_without_writing_next_to_it() {
    common::require_texlive!();
    let fixture = common::texlive_fixture("minimal");
    let entry = fixture.join("main.tex");
    let out = tempfile::tempdir().unwrap();
    let (code, doc, stderr) = run_json(
        out.path(),
        &[
            "compile",
            "--json",
            "--no-preview",
            "-o",
            out.path().to_str().unwrap(),
            entry.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    assert!(out.path().join("main.pdf").is_file());
    assert!(!fixture.join("texrun-out").exists());
}

/// Also needs `mutool` or Poppler (both are in the dev container).
#[test]
fn texlive_renders_page_previews() {
    common::require_texlive!();
    common::require_preview_tools!();
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\nPage 1.\\newpage\nPage 2.\\newpage\n\
         Page 3.\n\\end{document}\n",
    )]);
    let (code, doc, stderr) = run_json(dir.path(), &["compile", "--json", "main.tex"]);
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    let preview = &doc["preview"];
    assert_eq!(preview["status"], "rendered", "{preview:#}");
    assert_eq!(preview["pdf"]["page_count"], 3);
    assert_eq!(preview["pages"].as_array().unwrap().len(), 3);
    let png = dir.path().join("texrun-out/preview/page-003.png");
    assert!(fs::read(&png).unwrap().starts_with(b"\x89PNG"));

    let (code, doc, _) = run_json(
        dir.path(),
        &[
            "compile",
            "--json",
            "--pages",
            "2",
            "--preview-dpi",
            "72",
            "main.tex",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(doc["preview"]["pages"][0]["page"], 2);
    assert_eq!(doc["preview"]["pages"][0]["dpi"], 72);
}

#[test]
fn texlive_reports_located_errors() {
    common::require_texlive!();
    let dir = project(&[
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\input{chapters/intro}\n\\end{document}\n",
        ),
        ("chapters/intro.tex", "First line.\n\n\\undefinedmacro\n"),
    ]);
    let (code, doc, stderr) = run_json(dir.path(), &["compile", "--json", "main.tex"]);
    assert_eq!(code, 1, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "failed");
    let error = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["severity"] == "error")
        .expect("an error diagnostic");
    assert_eq!(error["kind"], "undefined_control_sequence");
    assert_eq!(error["file"], "chapters/intro.tex");
    assert_eq!(error["line"], 3);
    assert!(
        doc["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "log")
    );

    let out = texrun(dir.path(), &["compile", "main.tex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("chapters/intro.tex:3: error: Undefined control sequence"),
        "{text}"
    );
    // `-halt-on-error` stops TeX after it: still one error.
    assert!(text.contains(" (1 error, 0 warnings)\n"), "{text}");
    assert!(!text.contains("Fatal error"), "{text}");
}

#[test]
fn texlive_missing_package_is_one_located_error() {
    common::require_texlive!();
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\usepackage{amsmath}\n\\usepackage[draft]{texrunnonexistentpackage}\n% a comment\n\n\\begin{document}\nHi\n\\end{document}\n",
    )]);
    let out = texrun(dir.path(), &["compile", "main.tex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(
            "main.tex:3: error: LaTeX Error: File `texrunnonexistentpackage.sty' not found.\n"
        ),
        "{text}"
    );
    assert!(text.contains(" (1 error, 0 warnings)\n"), "{text}");
    assert!(!text.contains("Emergency stop"), "{text}");

    let (_, doc, stderr) = run_json(dir.path(), &["compile", "--json", "main.tex"]);
    let diagnostics = doc["diagnostics"].as_array().unwrap();
    let severities: Vec<_> = diagnostics
        .iter()
        .map(|d| (d["severity"].as_str(), d["kind"].as_str()))
        .collect();
    assert_eq!(
        severities,
        [
            (Some("error"), Some("missing_file")),
            (Some("info"), Some("emergency_stop"))
        ],
        "{doc:#}\n{stderr}"
    );
    assert_eq!(diagnostics[0]["line"], 3);
}

#[test]
fn texlive_timeout_exits_4() {
    common::require_texlive!();
    let fixture = common::texlive_fixture("timeout");
    let entry = fixture.join("main.tex");
    let out = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let (code, doc, stderr) = run_json(
        out.path(),
        &timeout_fixture_args(out.path().to_str().unwrap(), entry.to_str().unwrap(), "2s"),
    );
    assert_eq!(code, 4, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "timed_out");
    assert_eq!(doc["texrun_exit_code"], 4);
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(!fixture.join("texrun-out").exists());
}

#[test]
fn texlive_sigint_stops_pdflatex() {
    common::require_texlive!();
    let fixture = common::texlive_fixture("timeout");
    let entry = fixture.join("main.tex");
    let out = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let child = texrun(
        out.path(),
        &timeout_fixture_args(out.path().to_str().unwrap(), entry.to_str().unwrap(), "60s"),
    )
    .env("TMPDIR", tmp.path())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // Let latexmk start pdflatex.
    std::thread::sleep(Duration::from_secs(3));
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["outcome"], "cancelled");
    // Workspace, rc and probe directories are gone: cleanup ran.
    let left: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A cgroup this test may create cgroups in (docs/development.md, "cgroup
/// test"), or `None` (skipped) unless `TEXRUN_REQUIRE_CGROUP=1`.
#[cfg(target_os = "linux")]
fn cgroup_parent() -> Option<std::path::PathBuf> {
    let cgroups = texrun_process::Cgroups::detect();
    match cgroups.check() {
        Ok(()) => Some(cgroups.parent().unwrap().to_owned()),
        Err(reason) if std::env::var_os("TEXRUN_REQUIRE_CGROUP").is_some_and(|v| v == "1") => {
            panic!("TEXRUN_REQUIRE_CGROUP=1, but no cgroup can be used: {reason}")
        }
        Err(reason) => {
            eprintln!("skipped: no delegated cgroup ({reason})");
            None
        }
    }
}

/// What a compile run alone in a fresh cgroup did to that cgroup.
#[cfg(target_os = "linux")]
struct OwnCgroupRun {
    code: Option<i32>,
    doc: Value,
    stderr: String,
    /// `cgroup.subtree_control` of the cgroup after the run.
    subtree_control: String,
    /// Child cgroups texrun left in it (its `.main` leaf).
    children: Vec<String>,
}

/// Runs `texrun compile --json <args> main.tex` as the only process of a
/// new cgroup below `parent`, marked as delegated (`trusted.delegate=1`,
/// as systemd does for `Delegate=yes`) or not, and removes the cgroup.
#[cfg(target_os = "linux")]
fn compile_in_own_cgroup(parent: &Path, delegated: bool, args: &[&str]) -> OwnCgroupRun {
    let own = parent.join(format!(
        "texrun-cli-test-{}-{delegated}",
        std::process::id()
    ));
    fs::create_dir(&own).unwrap();
    if delegated {
        rustix::fs::setxattr(
            &own,
            "trusted.delegate",
            b"1",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
    }
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n",
    )]);
    let out = Command::new("/bin/sh")
        .args(["-c", "echo $$ > \"$0/cgroup.procs\" && exec \"$@\""])
        .arg(&own)
        .args([env!("CARGO_BIN_EXE_texrun"), "compile", "--json"])
        .args(args)
        .arg("main.tex")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let subtree_control = fs::read_to_string(own.join("cgroup.subtree_control")).unwrap();
    let mut children = Vec::new();
    for entry in fs::read_dir(&own).unwrap().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            children.push(entry.file_name().to_string_lossy().into_owned());
            let _ = fs::remove_dir(entry.path());
        }
    }
    let _ = fs::remove_dir(&own);
    assert!(!own.exists(), "the test cgroup was removed");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let doc =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {stderr}"));
    OwnCgroupRun {
        code: out.status.code(),
        doc,
        stderr,
        subtree_control,
        children,
    }
}

/// texrun started alone in a delegated cgroup (as with `systemd-run --user
/// --scope -p Delegate=yes`) moves itself into a leaf of it and runs the
/// engine and the preview tools in cgroups of their own.
#[cfg(target_os = "linux")]
#[test]
fn texlive_uses_a_delegated_cgroup_of_its_own() {
    common::require_texlive!();
    let Some(parent) = cgroup_parent() else {
        return;
    };
    let run = compile_in_own_cgroup(&parent, true, &["--cgroup", "required"]);
    let doc = &run.doc;
    assert_eq!(run.code, Some(0), "{doc:#}\n{}", run.stderr);
    assert_eq!(
        doc["resource_limits"],
        serde_json::json!({ "rlimits": true, "cgroup": true }),
        "{doc:#}"
    );
    assert!(
        doc["preview"]["notices"]
            .as_array()
            .is_none_or(|n| n.iter().all(|n| n["kind"] != "resource_limits")),
        "{doc:#}"
    );
    assert!(
        run.subtree_control.contains("memory"),
        "{}",
        run.subtree_control
    );
    assert_eq!(run.children.len(), 1, "{:?}", run.children);
    assert_eq!(
        run.children[0].rsplit_once('.').map(|(_, kind)| kind),
        Some("main"),
        "{:?}",
        run.children
    );
}

/// A cgroup that is writable (texrun runs as root here) but carries no
/// delegation marker is left alone: texrun neither moves itself into a
/// leaf nor enables controllers there. (It may still use the container's
/// cgroup namespace root.)
#[cfg(target_os = "linux")]
#[test]
fn texlive_leaves_a_cgroup_that_was_not_delegated_alone() {
    common::require_texlive!();
    let Some(parent) = cgroup_parent() else {
        return;
    };
    let run = compile_in_own_cgroup(&parent, false, &["--cgroup", "auto"]);
    assert_eq!(run.code, Some(0), "{:#}\n{}", run.doc, run.stderr);
    assert_eq!(run.subtree_control.trim(), "", "controllers were enabled");
    assert!(run.children.is_empty(), "{:?}", run.children);
}
