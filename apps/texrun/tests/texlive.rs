//! End-to-end tests of `texrun compile` against a real TeX Live + latexmk.
//!
//! `#[ignore]`d until #10 decides how TeX Live tests are enabled. Run them in
//! the dev container (docs/development.md):
//!
//! ```text
//! docker compose run --rm dev cargo test -p texrun -- --ignored
//! ```
//!
//! They fail (rather than skip) when latexmk is missing.

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

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn texlive_compiles_a_document_with_includes() {
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
    assert!(
        doc["engine"]["version"]
            .as_str()
            .unwrap()
            .starts_with("latexmk ")
    );
    let pdf = Path::new(doc["output_dir"].as_str().unwrap()).join("main.pdf");
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    assert!(dir.path().join("texrun-out/main.pdf").is_file());

    // Human-readable mode.
    let out = texrun(dir.path(), &["compile", "main.tex"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("PDF: texrun-out/main.pdf"), "{text}");
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn texlive_reports_located_errors() {
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
}

const INFINITE_LOOP: &str =
    "\\documentclass{article}\n\\begin{document}\n\\def\\x{\\x}\\x\n\\end{document}\n";

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn texlive_timeout_exits_4() {
    let dir = project(&[("main.tex", INFINITE_LOOP)]);
    let start = Instant::now();
    let (code, doc, stderr) = run_json(
        dir.path(),
        &["compile", "--json", "--timeout", "2s", "main.tex"],
    );
    assert_eq!(code, 4, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "timed_out");
    assert!(start.elapsed() < Duration::from_secs(30));
}

#[test]
#[ignore = "requires TeX Live with latexmk; run with --ignored (see docs/development.md)"]
fn texlive_sigint_stops_pdflatex() {
    let dir = project(&[("main.tex", INFINITE_LOOP)]);
    let tmp = tempfile::tempdir().unwrap();
    let child = texrun(
        dir.path(),
        &["compile", "--json", "--timeout", "60s", "main.tex"],
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
