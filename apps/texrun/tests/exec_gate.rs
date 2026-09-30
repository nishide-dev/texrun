//! The exec gate hosted by the texrun binary (`texrun __exec-gate`), as the
//! CLI uses it for the preview tools: stand-in tools (small `sh` scripts)
//! record the limits they start with.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use texrun_preview::{BackendChoice, ExecGate, PreviewOptions, PreviewStatus, Previewer, Toolset};

const TEXRUN: &str = env!("CARGO_BIN_EXE_texrun");

fn gate() -> ExecGate {
    ExecGate::new(TEXRUN).with_args(["__exec-gate"])
}

const PDFINFO: &str =
    "#!/bin/sh\necho 'Pages:           1'\necho 'Page    1 size:  300 x 400 pts'\n";

/// A `pdftoppm` that records the limits of itself and of a child it starts
/// at once (no waiting), then writes a tiny PNG header to `<prefix>.png`.
#[cfg(target_os = "linux")]
const RECORD: &str = "cat /proc/$$/limits > \"$HOME/../../limits.txt\"; \
                      echo --- >> \"$HOME/../../limits.txt\"; \
                      cat /proc/self/limits >> \"$HOME/../../limits.txt\"";
#[cfg(not(target_os = "linux"))]
const RECORD: &str = "{ ulimit -f; ulimit -c; echo ---; /bin/sh -c 'ulimit -f; ulimit -c'; } \
                      > \"$HOME/../../limits.txt\"";

fn pdftoppm() -> String {
    format!(
        "#!/bin/sh\n{RECORD}\nfor a; do last=$a; done\n\
         printf '\\211PNG\\r\\n\\032\\n\\000\\000\\000\\rIHDR\\000\\000\\000\\002\\000\\000\\000\\003' \
         > \"$last.png\"\n"
    )
}

fn fake_tools(bin: &Path) -> Toolset {
    for (name, script) in [("pdfinfo", PDFINFO.to_owned()), ("pdftoppm", pdftoppm())] {
        let path = bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut search = OsString::from(bin);
    search.push(":/usr/bin:/bin");
    Toolset::from_search_path(Some(&search))
}

fn render_once(tools: &Toolset, out: &Path) -> String {
    let pdf = out.join("in.pdf");
    fs::write(&pdf, "%PDF-1.4\n").unwrap();
    let report = Previewer::new(tools.clone())
        .with_exec_gate(gate())
        .render(
            &pdf,
            out,
            &PreviewOptions::default()
                .with_backend(BackendChoice::Poppler)
                .with_pages("1".parse().unwrap()),
        )
        .unwrap();
    assert_eq!(
        report.status,
        PreviewStatus::Rendered,
        "{:?}",
        report.notices
    );
    fs::read_to_string(out.join("limits.txt")).unwrap()
}

/// The whole 128 MiB budget is left for page 1, plus one byte.
const FILE_SIZE: u64 = (128 << 20) + 1;

fn check(limits: &str) {
    let (leader, child) = limits
        .split_once("---\n")
        .unwrap_or_else(|| panic!("{limits}"));
    for part in [leader, child] {
        #[cfg(target_os = "linux")]
        {
            let limit = |name: &str| {
                let line = part
                    .lines()
                    .find(|l| l.starts_with(name))
                    .unwrap_or_else(|| panic!("{name} missing in {part}"));
                // Columns: name, soft, hard, units; take the hard limit.
                line.split_whitespace().rev().nth(1).unwrap().to_owned()
            };
            assert_eq!(
                limit("Max address space"),
                (2u64 << 30).to_string(),
                "{limits}"
            );
            assert_eq!(limit("Max file size"), FILE_SIZE.to_string(), "{limits}");
            assert_eq!(limit("Max core file size"), "0", "{limits}");
        }
        #[cfg(not(target_os = "linux"))]
        {
            // `ulimit -f` counts 512- or 1024-byte blocks, rounded down.
            let lines: Vec<_> = part.lines().collect();
            let blocks = [
                (FILE_SIZE / 512).to_string(),
                (FILE_SIZE / 1024).to_string(),
            ];
            assert!(blocks.contains(&lines[0].to_owned()), "{limits}");
            assert_eq!(lines[1], "0", "{limits}");
        }
    }
}

#[test]
fn preview_tools_start_with_their_limits() {
    let bin = tempfile::tempdir().unwrap();
    let tools = fake_tools(bin.path());
    for _ in 0..20 {
        let out = tempfile::tempdir().unwrap();
        check(&render_once(&tools, out.path()));
    }
}

/// A required gate that passes the check up front but is gone when the
/// next tool starts (e.g. the binary was removed meanwhile): that tool is
/// not run, and the notice says it is about the resource limits.
#[test]
fn a_required_gate_that_disappears_is_a_resource_limits_notice() {
    let bin = tempfile::tempdir().unwrap();
    let copy = bin.path().join("texrun-copy");
    fs::copy(TEXRUN, &copy).unwrap();
    // `pdfinfo` runs through the gate, then removes it.
    let pdfinfo = PDFINFO.replace(
        "#!/bin/sh\n",
        &format!("#!/bin/sh\nrm -f '{}'\n", copy.display()),
    );
    let tools = fake_tools(bin.path());
    fs::write(bin.path().join("pdfinfo"), pdfinfo).unwrap();
    let out = tempfile::tempdir().unwrap();
    let pdf = out.path().join("in.pdf");
    fs::write(&pdf, "%PDF-1.4\n").unwrap();
    let report = Previewer::new(tools)
        .with_exec_gate(
            ExecGate::new(&copy)
                .with_args(["__exec-gate"])
                .with_required(true),
        )
        .render(
            &pdf,
            out.path(),
            &PreviewOptions::default()
                .with_backend(BackendChoice::Poppler)
                .with_pages("1".parse().unwrap()),
        )
        .unwrap();
    assert!(!copy.exists(), "pdfinfo ran through the gate");
    assert_eq!(
        report.status,
        PreviewStatus::Skipped,
        "{:?}",
        report.notices
    );
    let json = serde_json::to_value(&report).unwrap();
    let notices = json["notices"].as_array().unwrap();
    assert_eq!(notices.len(), 1, "{json}");
    assert_eq!(notices[0]["kind"], "resource_limits", "{json}");
    assert!(
        notices[0]["message"]
            .as_str()
            .unwrap()
            .contains("texrun-copy"),
        "{json}"
    );
    assert!(!out.path().join("limits.txt").exists(), "pdftoppm ran");
}

#[test]
fn the_gate_subcommand_refuses_other_arguments_and_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["__exec-gate"][..],
        &["__exec-gate", "--help"],
        &["__exec-gate", "--", "/usr/bin/touch", "ran"],
    ] {
        let out = Command::new(TEXRUN)
            .args(args)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("texrun exec gate:"),
            "{args:?}"
        );
    }
    assert!(!dir.path().join("ran").exists());

    let help = Command::new(TEXRUN).arg("--help").output().unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("exec-gate"));
}
