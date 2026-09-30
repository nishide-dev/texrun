//! Behaviour of the `texrun` binary without TeX: help, exit codes, the JSON
//! document, runtime errors and cancellation, using a stand-in `latexmk`
//! shell script found through `PATH`.
//!
//! Every test runs texrun with its own `TMPDIR` (so leftover workspaces can
//! be detected) and `HOME`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const MINIMAL: &str = "\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n";

/// Common head of the fake latexmk: answers `-norc -v` like latexmk 4.86
/// and finds `-outdir=` and the job name.
const FAKE_HEAD: &str = r#"#!/bin/sh
if [ "$1" = "-norc" ] && [ "$2" = "-v" ]; then
  echo "Latexmk, John Collins, 11 Dec. 2024. Version 4.86"
  exit 0
fi
for a in "$@"; do
  case "$a" in -outdir=*) out="${a#-outdir=}";; esac
  last="$a"
done
stem=$(basename "$last" .tex)
"#;

const FAKE_SUCCEED: &str = r#"
printf '%%PDF-1.4 fake\n' > "$out/$stem.pdf"
printf 'This is pdfTeX (fake)\n(./%s.tex)\n' "$stem" > "$out/$stem.log"
exit 0
"#;

const FAKE_FAIL: &str = r#"
printf 'This is pdfTeX (fake)\n file:line:error style messages enabled.\n(./%s.tex\nLaTeX Warning: Reference `x\342\200\256red'"'"' on page 1 undefined on input line 7.\n\n./%s.tex:5: Undefined control sequence.\nl.5 \\foo\n\n)\n' "$stem" "$stem" > "$out/$stem.log"
exit 12
"#;

struct Env {
    base: TempDir,
}

impl Env {
    fn new(behaviour: &str) -> Self {
        let base = tempfile::tempdir().unwrap();
        for dir in ["bin", "tmp", "home", "proj"] {
            fs::create_dir(base.path().join(dir)).unwrap();
        }
        let env = Self { base };
        // PATH is only this directory, so no real latexmk or preview tool
        // of the host is found; the fake scripts get the few commands they
        // use as symlinks.
        for tool in ["basename", "date", "env", "sleep"] {
            let real = ["/usr/bin", "/bin"]
                .iter()
                .map(|d| Path::new(d).join(tool))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("{tool} not found"));
            std::os::unix::fs::symlink(real, env.path("bin").join(tool)).unwrap();
        }
        env.write_latexmk(behaviour);
        env
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.base.path().join(rel)
    }

    fn write_latexmk(&self, behaviour: &str) {
        self.write_tool("latexmk", &format!("{FAKE_HEAD}{behaviour}"));
    }

    fn write_tool(&self, name: &str, script: &str) {
        let fake = self.path("bin").join(name);
        fs::write(&fake, script).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn remove_latexmk(&self) {
        fs::remove_file(self.path("bin/latexmk")).unwrap();
    }

    fn file(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_texrun"));
        cmd.args(args)
            .current_dir(self.path("proj"))
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("TMPDIR", self.path("tmp"))
            .env("HOME", self.path("home"));
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Entries texrun left in its temporary directory.
    fn leftovers(&self) -> Vec<String> {
        fs::read_dir(self.path("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("texrun exited normally")
}

#[test]
fn help_documents_commands_options_and_exit_codes() {
    let env = Env::new(FAKE_SUCCEED);
    let top = env.run(&["--help"]);
    assert_eq!(code(&top), 0);
    let text = stdout(&top);
    assert!(text.contains("compile"), "{text}");
    assert!(text.contains("Exit codes:"), "{text}");

    let compile = env.run(&["compile", "--help"]);
    assert_eq!(code(&compile), 0);
    let text = stdout(&compile);
    for needle in [
        "<ENTRYPOINT>",
        "--json",
        "--output <DIR>",
        "--root <DIR>",
        "--timeout <DURATION>",
        "[default: 60s]",
        "--keep-workspace",
        "--source-date-epoch <SECONDS>",
        "--no-preview",
        "--pages <RANGE>",
        "--preview-dpi <DPI>",
        "--preview-backend <BACKEND>",
        "130",
    ] {
        assert!(text.contains(needle), "missing {needle}:\n{text}");
    }

    let version = env.run(&["--version"]);
    assert_eq!(code(&version), 0);
    assert!(stdout(&version).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn usage_errors_exit_2_and_report_json_when_requested() {
    let env = Env::new(FAKE_SUCCEED);
    let none = env.run(&[]);
    assert_eq!(code(&none), 2);

    let plain = env.run(&["compile", "--bogus", "main.tex"]);
    assert_eq!(code(&plain), 2);
    assert!(plain.stdout.is_empty());
    assert!(stderr(&plain).contains("--bogus"));

    let with_json = env.run(&["compile", "--json", "--timeout", "0", "main.tex"]);
    assert_eq!(code(&with_json), 2);
    let doc = json(&with_json);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["error"]["stage"], "args");
    assert_eq!(doc["error"]["kind"], "usage");
    assert_eq!(doc["error"]["category"], "usage");
    assert!(doc.get("outcome").is_none());

    let pages = env.run(&["compile", "--json", "--pages", "3-1", "main.tex"]);
    assert_eq!(code(&pages), 2);
    let doc = json(&pages);
    assert_eq!(doc["error"]["kind"], "usage");
    let message = doc["error"]["message"].as_str().unwrap();
    assert!(message.contains("invalid page range"), "{message}");
}

#[test]
fn success_copies_the_pdf_next_to_the_entrypoint() {
    let env = Env::new(FAKE_SUCCEED);
    env.file("proj/paper/main.tex", MINIMAL);

    let out = env.run(&["compile", "paper/main.tex"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("Compiled paper/main.tex in "),
        "{text}"
    );
    assert_eq!(lines[1], "  PDF: paper/texrun-out/main.pdf");
    // No preview tool: a warning, but still a success.
    assert!(
        lines[2].starts_with("warning: preview: no PDF preview tool was found"),
        "{text}"
    );
    assert!(env.path("proj/paper/texrun-out/main.pdf").is_file());
    assert!(env.path("proj/paper/texrun-out/main.log").is_file());
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());

    // A second run replaces the outputs and does not copy texrun-out/ in.
    let doc = json(&env.run(&["compile", "--json", "paper/main.tex"]));
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["outcome"], "succeeded");
    assert_eq!(doc["engine"]["name"], "texlive");
    assert_eq!(doc["engine"]["version"], "latexmk 4.86");
    assert_eq!(doc["exit"]["code"], 0);
    assert!(doc["elapsed_ms"].is_u64());
    assert_eq!(doc["diagnostics"], Value::Array(vec![]));
    assert_eq!(doc["artifacts"][0]["kind"], "pdf");
    assert_eq!(doc["artifacts"][0]["path"], "main.pdf");
    assert_eq!(doc["artifacts"][0]["size_bytes"], 14);
    assert_eq!(doc["artifacts"][1]["kind"], "log");
    let output_dir = PathBuf::from(doc["output_dir"].as_str().unwrap());
    assert!(output_dir.is_absolute());
    assert_eq!(
        output_dir,
        fs::canonicalize(env.path("proj/paper/texrun-out")).unwrap()
    );
    assert!(output_dir.join("main.pdf").is_file());
    assert_eq!(doc["project"]["entrypoint"], "main.tex");
    assert_eq!(
        PathBuf::from(doc["project"]["root"].as_str().unwrap()),
        fs::canonicalize(env.path("proj/paper")).unwrap()
    );
    let excluded = doc["workspace"]["excluded"].as_array().unwrap();
    assert!(
        excluded
            .iter()
            .any(|e| e["path"] == "texrun-out" && e["reason"] == "excluded_name"),
        "{excluded:?}"
    );
    assert!(doc.get("error").is_none());
    assert_eq!(doc["preview"]["status"], "skipped");
    assert_eq!(doc["preview"]["notices"][0]["kind"], "tool_unavailable");

    let doc = json(&env.run(&["compile", "--json", "--no-preview", "paper/main.tex"]));
    assert!(doc.get("preview").is_none(), "{doc}");
}

/// `pdfinfo` output for a 3-page document (as in texrun-preview's tests).
const FAKE_PDFINFO: &str = r#"#!/bin/sh
echo "Pages:           3"
for p in 1 2 3; do
  echo "Page    $p size:  300 x 400 pts"
  echo "Page    $p rot:   0"
done
"#;

/// A `pdftoppm` that writes a tiny PNG header (2 x 3 px) to `<prefix>.png`.
const FAKE_PDFTOPPM: &str = r#"#!/bin/sh
for a; do last=$a; done
printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\000\002\000\000\000\003' > "$last.png"
"#;

#[test]
fn previews_are_rendered_and_collected_after_success() {
    let env = Env::new(FAKE_SUCCEED);
    env.write_tool("pdfinfo", FAKE_PDFINFO);
    env.write_tool("pdftoppm", FAKE_PDFTOPPM);
    env.file("proj/main.tex", MINIMAL);

    let out = env.run(&[
        "compile",
        "--json",
        "--preview-backend",
        "poppler",
        "main.tex",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json(&out);
    let preview = &doc["preview"];
    assert_eq!(preview["status"], "rendered", "{preview:#}");
    assert_eq!(preview["backend"], "poppler");
    assert_eq!(preview["pdf"]["page_count"], 3);
    assert_eq!(preview["pages"][0]["path"], "preview/page-001.png");
    assert_eq!(preview["pages"][0]["width_px"], 2);
    let previews: Vec<&Value> = doc["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "preview")
        .collect();
    assert_eq!(previews.len(), 3);
    assert_eq!(previews[2]["page"], 3);
    let output_dir = Path::new(doc["output_dir"].as_str().unwrap());
    assert!(output_dir.join("preview/page-003.png").is_file());
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());

    let out = env.run(&["compile", "--pages", "2-", "main.tex"]);
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    assert!(
        text.contains(
            "  previews: texrun-out/preview/page-002.png .. page-003.png (2 of 3 pages, poppler)"
        ),
        "{text}"
    );

    // Not after a failed compile.
    env.write_latexmk(FAKE_FAIL);
    let doc = json(&env.run(&["compile", "--json", "main.tex"]));
    assert_eq!(doc["outcome"], "failed");
    assert!(doc.get("preview").is_none(), "{doc}");
}

#[test]
fn output_and_root_options() {
    let env = Env::new(FAKE_SUCCEED);
    env.file("proj/src/doc.tex", MINIMAL);
    let out = env.run(&[
        "compile",
        "--json",
        "--root",
        ".",
        "--output",
        "build",
        "src/doc.tex",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["project"]["entrypoint"], "src/doc.tex");
    assert_eq!(doc["artifacts"][0]["path"], "doc.pdf");
    assert!(env.path("proj/build/doc.pdf").is_file());

    // `build/` is directly in the root and is not copied back in.
    let doc = json(&env.run(&[
        "compile",
        "--json",
        "--root",
        ".",
        "-o",
        "build",
        "src/doc.tex",
    ]));
    let excluded = doc["workspace"]["excluded"].as_array().unwrap();
    assert!(
        excluded.iter().any(|e| e["path"] == "build"),
        "{excluded:?}"
    );

    // The entrypoint must be inside --root.
    env.file("other/x.tex", MINIMAL);
    let out = env.run(&["compile", "--json", "--root", "src", "../other/x.tex"]);
    assert_eq!(code(&out), 2);
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "entrypoint_outside_root");
    assert_eq!(doc["error"]["stage"], "project");
    assert!(doc["error"]["hint"].is_string());
}

#[test]
fn document_failure_exits_1_with_diagnostics() {
    let env = Env::new(FAKE_FAIL);
    env.file("proj/main.tex", MINIMAL);

    let out = env.run(&["compile", "main.tex"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("main.tex:5: error: "), "{text}");
    // The bidi override from the log is shown escaped (the log parser
    // already replaces control characters).
    assert!(text.contains("x\\u{202E}red"), "{text}");
    assert!(!text.contains('\u{202e}'));
    assert!(text.contains("Failed to compile main.tex in "), "{text}");
    assert!(text.contains("  log: texrun-out/main.log"), "{text}");
    assert!(!env.path("proj/texrun-out/main.pdf").exists());

    let doc = json(&env.run(&["compile", "--json", "main.tex"]));
    assert_eq!(doc["outcome"], "failed");
    assert_eq!(doc["exit"]["code"], 12);
    let diag = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["severity"] == "error")
        .expect("an error diagnostic");
    assert_eq!(diag["kind"], "undefined_control_sequence");
    assert_eq!(diag["file"], "main.tex");
    assert_eq!(diag["line"], 5);
    // JSON keeps the raw text.
    let warning = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["severity"] == "warning")
        .expect("a warning diagnostic");
    assert!(
        warning["message"]
            .as_str()
            .unwrap()
            .contains("x\u{202e}red"),
        "{warning}"
    );
    assert_eq!(doc["artifacts"][0]["kind"], "log");
}

#[test]
fn missing_latexmk_is_a_runtime_error() {
    let env = Env::new(FAKE_SUCCEED);
    env.remove_latexmk();
    env.file("proj/main.tex", MINIMAL);

    let out = env.run(&["compile", "--json", "main.tex"]);
    assert_eq!(code(&out), 3);
    let doc = json(&out);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["error"]["stage"], "probe");
    assert_eq!(doc["error"]["kind"], "unavailable");
    assert_eq!(doc["error"]["category"], "runtime");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("latexmk")
    );
    assert!(doc.get("outcome").is_none());
    assert_eq!(doc["project"]["entrypoint"], "main.tex");

    let plain = env.run(&["compile", "main.tex"]);
    assert_eq!(code(&plain), 3);
    assert!(plain.stdout.is_empty());
    assert!(stderr(&plain).starts_with("error: engine `texlive` is unavailable"));
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[test]
fn input_errors_exit_2() {
    let env = Env::new(FAKE_SUCCEED);
    let out = env.run(&["compile", "--json", "missing.tex"]);
    assert_eq!(code(&out), 2);
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "entrypoint_not_found");
    assert_eq!(doc["error"]["category"], "input");

    let plain = env.run(&["compile", "missing.tex"]);
    assert_eq!(code(&plain), 2);
    assert!(stderr(&plain).starts_with("error: "));
}

#[test]
fn home_and_temp_roots_need_an_explicit_root() {
    let env = Env::new(FAKE_SUCCEED);
    let in_home = env.file("home/main.tex", MINIMAL);
    let out = env.run(&["compile", "--json", in_home.to_str().unwrap()]);
    assert_eq!(code(&out), 2);
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "unsafe_root");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("home directory")
    );

    let in_tmp = env.file("tmp/main.tex", MINIMAL);
    let out = env.run(&["compile", "--json", in_tmp.to_str().unwrap()]);
    assert_eq!(code(&out), 2);
    assert_eq!(json(&out)["error"]["kind"], "unsafe_root");

    // Explicit --root is accepted, with a warning.
    let home = env.path("home");
    let out = env.run(&[
        "compile",
        "--root",
        home.to_str().unwrap(),
        in_home.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stderr(&out).contains("home directory"), "{}", stderr(&out));
}

#[test]
fn excluded_tool_config_is_reported() {
    let env = Env::new(FAKE_SUCCEED);
    env.file("proj/main.tex", MINIMAL);
    env.file("proj/latexmkrc", "$pdflatex = 'evil';\n");

    let out = env.run(&["compile", "main.tex"]);
    assert_eq!(code(&out), 0);
    assert!(
        stderr(&out).contains("warning: not copied into the workspace: latexmkrc"),
        "{}",
        stderr(&out)
    );

    let out = env.run(&["compile", "--json", "main.tex"]);
    let doc = json(&out);
    let excluded = doc["workspace"]["excluded"].as_array().unwrap();
    assert!(
        excluded
            .iter()
            .any(|e| e["path"] == "latexmkrc" && e["reason"] == "tool_config"),
        "{excluded:?}"
    );
    // The warning goes to stderr even with --json.
    assert!(stderr(&out).contains("latexmkrc"));
}

/// Writes `marker` when started, then keeps touching a heartbeat file.
fn fake_hang(marker: &Path, heartbeat: &Path) -> String {
    format!(
        "\necho started > '{}'\nwhile :; do date +%s%N >> '{}'; sleep 0.1; done\n",
        marker.display(),
        heartbeat.display()
    )
}

fn wait_for(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "{} did not appear",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_stopped(heartbeat: &Path) {
    let before = fs::read_to_string(heartbeat).unwrap_or_default().len();
    std::thread::sleep(Duration::from_millis(500));
    let after = fs::read_to_string(heartbeat).unwrap_or_default().len();
    assert_eq!(before, after, "the fake latexmk is still running");
}

#[test]
fn timeout_exits_4() {
    let env = Env::new("");
    let (marker, heartbeat) = (env.path("started"), env.path("heartbeat"));
    env.write_latexmk(&fake_hang(&marker, &heartbeat));
    env.file("proj/main.tex", MINIMAL);

    let out = env.run(&["compile", "--json", "--timeout", "1s", "main.tex"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(json(&out)["outcome"], "timed_out");
    assert_stopped(&heartbeat);
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());

    let out = env.run(&["compile", "--timeout", "500ms", "main.tex"]);
    assert_eq!(code(&out), 4);
    assert!(
        stdout(&out).contains("Timed out compiling main.tex after"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn sigint_cancels_kills_latexmk_and_cleans_up() {
    for (signal, expected) in [("-INT", 130), ("-TERM", 143)] {
        let env = Env::new("");
        let (marker, heartbeat) = (env.path("started"), env.path("heartbeat"));
        env.write_latexmk(&fake_hang(&marker, &heartbeat));
        env.file("proj/main.tex", MINIMAL);

        let child = env
            .command(&["compile", "--json", "--timeout", "60s", "main.tex"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for(&marker, Duration::from_secs(20));
        let status = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let out = child.wait_with_output().unwrap();

        assert_eq!(code(&out), expected, "{}", stderr(&out));
        assert_eq!(json(&out)["outcome"], "cancelled");
        assert!(stderr(&out).contains("stopping the compile"));
        assert_stopped(&heartbeat);
        assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
    }
}

#[test]
fn keep_workspace_prints_and_keeps_the_directory() {
    let env = Env::new(FAKE_SUCCEED);
    env.file("proj/main.tex", MINIMAL);
    let out = env.run(&["compile", "--json", "--keep-workspace", "main.tex"]);
    assert_eq!(code(&out), 0);
    let doc = json(&out);
    let kept = PathBuf::from(doc["workspace"]["kept_path"].as_str().unwrap());
    assert!(kept.join("main.tex").is_file());
    assert!(
        stderr(&out).contains(&format!("keeping the workspace at {}", kept.display())),
        "{}",
        stderr(&out)
    );
    fs::remove_dir_all(kept).unwrap();
}

#[test]
fn source_date_epoch_is_only_set_by_the_option() {
    let env = Env::new("");
    let record = env.path("env.txt");
    env.write_latexmk(&format!("\nenv > '{}'\n{FAKE_SUCCEED}", record.display()));
    env.file("proj/main.tex", MINIMAL);

    let out = env
        .command(&["compile", "main.tex"])
        .env("SOURCE_DATE_EPOCH", "123")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let seen = fs::read_to_string(&record).unwrap();
    assert!(!seen.contains("SOURCE_DATE_EPOCH"), "{seen}");

    let out = env.run(&["compile", "--source-date-epoch", "0", "main.tex"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let seen = fs::read_to_string(&record).unwrap();
    assert!(seen.contains("SOURCE_DATE_EPOCH=0\n"), "{seen}");
    assert!(seen.contains("FORCE_SOURCE_DATE=1\n"), "{seen}");
}

#[test]
fn stale_symlink_in_output_is_refused() {
    let env = Env::new(FAKE_SUCCEED);
    env.file("proj/main.tex", MINIMAL);
    fs::create_dir(env.path("proj/texrun-out")).unwrap();
    let target = env.file("elsewhere.pdf", "keep me");
    std::os::unix::fs::symlink(&target, env.path("proj/texrun-out/main.pdf")).unwrap();

    let out = env.run(&["compile", "--json", "main.tex"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    let doc = json(&out);
    // The compile itself succeeded; copying its output did not.
    assert_eq!(doc["outcome"], "succeeded");
    assert_eq!(doc["error"]["stage"], "collect");
    assert_eq!(doc["error"]["kind"], "unsafe_output_path");
    assert_eq!(doc["artifacts"], Value::Array(vec![]));
    assert_eq!(fs::read_to_string(target).unwrap(), "keep me");
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}
