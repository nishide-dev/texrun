//! End-to-end tests of `texrun compile --backend container`.
//!
//! They need a container runtime and the engine image
//! (`docker build -t texrun-engine:latest docker/engine`, or the image in
//! `TEXRUN_SANDBOX_IMAGE`); without them they are skipped, unless
//! `TEXRUN_REQUIRE_SANDBOX=1` (CI's `sandbox` job). The engine's own
//! container tests are in `crates/texrun-texlive/tests/container.rs`.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
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

/// `texrun compile --json --backend container --container-image <image>
/// --no-preview <args>` in `dir`.
fn compile(dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let image = common::sandbox_image();
    let out: Output = Command::new(env!("CARGO_BIN_EXE_texrun"))
        .args([
            "compile",
            "--json",
            "--backend",
            "container",
            "--container-image",
            &image,
            "--no-preview",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
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
fn a_document_compiles_in_the_container() {
    common::require_sandbox!();
    let dir = project(&[
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\input{chapters/intro}\n\\end{document}\n",
        ),
        ("chapters/intro.tex", "Hello from the container.\n"),
    ]);
    let (code, doc, stderr) = compile(dir.path(), &["main.tex"]);
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "succeeded");
    assert_eq!(doc["engine"]["name"], "texlive-container");
    let version = doc["engine"]["version"].as_str().unwrap();
    assert!(version.starts_with("latexmk "), "{version}");
    // A local build has no version label (#54; CI's `sandbox` job builds
    // the image so).
    assert!(
        version.ends_with(", image without a version label)")
            || version.contains(", image version "),
        "{version}"
    );
    assert_eq!(
        doc["resource_limits"],
        serde_json::json!({ "rlimits": true, "cgroup": true })
    );
    assert!(
        fs::read(dir.path().join("texrun-out/main.pdf"))
            .unwrap()
            .starts_with(b"%PDF-")
    );
}

#[test]
fn diagnostics_point_into_the_project() {
    common::require_sandbox!();
    let dir = project(&[
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\input{sub/bad}\n\\end{document}\n",
        ),
        ("sub/bad.tex", "Fine.\n\\undefinedcommand\n"),
    ]);
    let (code, doc, stderr) = compile(dir.path(), &["main.tex"]);
    assert_eq!(code, 1, "{doc:#}\n{stderr}");
    let d = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["kind"] == "undefined_control_sequence")
        .unwrap_or_else(|| panic!("{doc:#}"));
    assert_eq!(d["file"], "sub/bad.tex", "{d:#}");
    assert_eq!(d["line"], 2, "{d:#}");
    // No host or container path in the result.
    let text = doc.to_string();
    assert!(!text.contains("/workspace/"), "{text}");
}

#[test]
fn a_timeout_is_exit_code_4() {
    common::require_sandbox!();
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\n\\def\\x{\\x}\\x\n\\end{document}\n",
    )]);
    let started = Instant::now();
    let (code, doc, stderr) = compile(dir.path(), &["--timeout", "2s", "main.tex"]);
    assert_eq!(code, 4, "{doc:#}\n{stderr}");
    assert_eq!(doc["outcome"], "timed_out");
    assert!(started.elapsed() < Duration::from_secs(40));
}

#[test]
fn a_missing_image_is_a_runtime_error() {
    // Also without a runtime: either way the backend is unavailable.
    let dir = project(&[("main.tex", "x")]);
    let out = Command::new(env!("CARGO_BIN_EXE_texrun"))
        .args([
            "compile",
            "--json",
            "--backend",
            "container",
            "--container-image",
            "texrun-no-such-image:0",
            "main.tex",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3), "{doc:#}");
    assert_eq!(doc["error"]["stage"], "probe", "{doc:#}");
    assert_eq!(doc["error"]["kind"], "unavailable", "{doc:#}");
    assert!(
        doc["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("docker pull ghcr.io/nishide-dev/texrun-engine:"),
        "{doc:#}"
    );
    // Nothing was compiled.
    assert!(!dir.path().join("texrun-out").exists());
}

#[test]
fn container_options_need_the_container_backend() {
    let dir = project(&[("main.tex", "x")]);
    for args in [
        &["--container-image", "x"][..],
        &["--container-runtime", "docker"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_texrun"))
            .arg("compile")
            .arg("--json")
            .args(args)
            .arg("main.tex")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
}

/// The directory of the first `docker` (or `podman`) on `PATH`.
fn runtime_dir() -> std::path::PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .find(|dir| dir.join("docker").is_file() || dir.join("podman").is_file())
        .expect("a container runtime on PATH")
}

#[test]
fn previews_are_rendered_in_the_container() {
    common::require_sandbox!();
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\nOne.\\newpage Two.\n\\end{document}\n",
    )]);
    // Only the runtime CLI on `PATH`: no preview tool of the host can be
    // found, so the images come from the container.
    let bin = tempfile::tempdir().unwrap();
    let source = runtime_dir();
    for name in ["docker", "podman"] {
        if source.join(name).is_file() {
            std::os::unix::fs::symlink(source.join(name), bin.path().join(name)).unwrap();
        }
    }
    let image = common::sandbox_image();
    let out = Command::new(env!("CARGO_BIN_EXE_texrun"))
        .args([
            "compile",
            "--json",
            "--backend",
            "container",
            "--container-image",
            &image,
            "main.tex",
        ])
        .env("PATH", bin.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(out.status.code(), Some(0), "{doc:#}");
    assert_eq!(doc["preview"]["status"], "rendered", "{doc:#}");
    assert_eq!(
        doc["preview"]["pages"].as_array().unwrap().len(),
        2,
        "{doc:#}"
    );
    for page in ["page-001.png", "page-002.png"] {
        let png = fs::read(dir.path().join("texrun-out/preview").join(page)).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "{page}");
    }
}

/// Runs the container runtime CLI with `args` and returns its stdout
/// (empty if it fails).
fn runtime(args: &[&str]) -> String {
    let dir = runtime_dir();
    let program = if dir.join("docker").is_file() {
        dir.join("docker")
    } else {
        dir.join("podman")
    };
    let out = Command::new(program).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The IDs of the containers created by the texrun process `pid`, with
/// `status` (`running`, `exited`, ...) if given.
fn containers_of(pid: u32, status: Option<&str>) -> Vec<String> {
    let label = format!("label=org.texrun.sandbox.pid={pid}");
    let status = status.map(|s| format!("status={s}"));
    let mut args = vec!["ps", "--all", "--quiet", "--no-trunc", "--filter", &label];
    if let Some(status) = &status {
        args.extend(["--filter", status]);
    }
    runtime(&args)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Waits (at most 60 s) until `ready` returns something.
fn wait_for<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `texrun compile --json --backend container <args> main.tex` in `dir` as
/// a child process, with `TMPDIR` set to `tmp`.
fn spawn_compile(dir: &Path, tmp: &Path, args: &[&str]) -> std::process::Child {
    let image = common::sandbox_image();
    Command::new(env!("CARGO_BIN_EXE_texrun"))
        .args([
            "compile",
            "--json",
            "--backend",
            "container",
            "--container-image",
            &image,
        ])
        .args(args)
        .arg("main.tex")
        .env("TMPDIR", tmp)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// A directory for texrun's `TMPDIR`, below the system temporary directory
/// (which the runtime's VM shares on macOS).
fn private_tmp() -> TempDir {
    tempfile::Builder::new()
        .prefix("texrun-it-tmp-")
        .tempdir()
        .unwrap()
}

/// The `texrun-preview-*` scratch directories in `tmp`.
fn scratch_dirs(tmp: &Path) -> Vec<String> {
    fs::read_dir(tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("texrun-preview-"))
        .collect()
}

const TRIVIAL: &str = "\\documentclass{article}\n\\begin{document}\nx\n\\end{document}\n";

/// texrun is killed (`SIGKILL`) during a compile: its container is left.
/// The next texrun keeps it while it runs and removes it once it stopped.
#[test]
fn a_container_left_by_a_killed_run_is_reclaimed_once_it_stopped() {
    common::require_sandbox!();
    let tmp = private_tmp();
    let looping = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\n\\def\\x{\\x}\\x\n\\end{document}\n",
    )]);
    let mut killed = spawn_compile(
        looping.path(),
        tmp.path(),
        &["--no-preview", "--timeout", "60s"],
    );
    let pid = killed.id();
    // The compile's container (not the probe's, which ends at once): the
    // same one running for a while.
    let left = wait_for("the compile container", || {
        let first = containers_of(pid, Some("running"));
        std::thread::sleep(Duration::from_millis(500));
        let second = containers_of(pid, Some("running"));
        (first.len() == 1 && first == second).then(|| first[0].clone())
    });
    killed.kill().unwrap();
    killed.wait().unwrap();

    // Still running (its deadline is far): the next run keeps it.
    let other = project(&[("main.tex", TRIVIAL)]);
    let (code, doc, stderr) = compile(other.path(), &["main.tex"]);
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    assert_eq!(
        containers_of(pid, Some("running")),
        std::slice::from_ref(&left)
    );

    // Stopped (as by its deadline): the next run removes it.
    runtime(&["kill", "--", &left]);
    // (The texrun of another test may remove it as soon as it stopped.)
    wait_for("the container to stop", || {
        containers_of(pid, Some("running")).is_empty().then_some(())
    });
    let (code, doc, stderr) = compile(other.path(), &["main.tex"]);
    assert_eq!(code, 0, "{doc:#}\n{stderr}");
    assert!(containers_of(pid, None).is_empty(), "{left} was kept");
}

/// texrun is killed during the preview: its session container and its
/// scratch directory are left. The next texrun removes both (the
/// container once it stopped).
#[test]
fn a_preview_left_by_a_killed_run_is_reclaimed() {
    common::require_sandbox!();
    let tmp = private_tmp();
    // Enough pages that the preview runs for seconds.
    let many = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\n\\count1=0\n\
         \\loop\\advance\\count1 by 1 Page \\the\\count1.\\newpage\\ifnum\\count1<200\\repeat\n\
         \\end{document}\n",
    )]);
    let mut killed = spawn_compile(many.path(), tmp.path(), &["--pages", "1-200"]);
    let pid = killed.id();
    // Kill it once its preview container runs (its scratch directory is
    // made before the container); the compile's container is gone by then.
    let scratch = wait_for("the preview scratch directory", || {
        scratch_dirs(tmp.path()).first().cloned()
    });
    let session = wait_for("the preview container", || {
        let running = containers_of(pid, Some("running"));
        (running.len() == 1).then(|| running[0].clone())
    });
    killed.kill().unwrap();
    killed.wait().unwrap();
    assert!(tmp.path().join(&scratch).is_dir(), "{scratch}");
    assert_eq!(containers_of(pid, None), std::slice::from_ref(&session));

    // The session container stops after its lifetime; stop it now.
    runtime(&["kill", "--", &session]);
    wait_for("the container to stop", || {
        containers_of(pid, Some("running")).is_empty().then_some(())
    });

    let other = project(&[("main.tex", TRIVIAL)]);
    let out = spawn_compile(other.path(), tmp.path(), &[])
        .wait_with_output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        other
            .path()
            .join("texrun-out/preview/page-001.png")
            .is_file(),
        "the next run rendered its own preview"
    );
    assert!(containers_of(pid, None).is_empty(), "{session} was kept");
    assert_eq!(scratch_dirs(tmp.path()), Vec::<String>::new());
}
