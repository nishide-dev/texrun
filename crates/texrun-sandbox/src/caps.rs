//! The capability sets of a process in the container, read from inside
//! (#47).
//!
//! The `HostConfig` check after `create` ([`Container::refusal`]) reads
//! what the runtime *recorded*: Docker records `--cap-drop ALL` as `ALL`,
//! but Podman lists the capabilities it dropped one by one, so for Podman
//! that check can only see that some were dropped. What a process in the
//! container actually has is in its `/proc/self/status`: the inheritable,
//! permitted, effective, bounding and ambient sets (`CapInh`, `CapPrm`,
//! `CapEff`, `CapBnd`, `CapAmb`). With `--cap-drop ALL` all of them are
//! empty, also the bounding set, which limits what a non-root process
//! could ever gain (e.g. through file capabilities, which
//! `no-new-privileges` also blocks).
//!
//! [`capability_report`] runs a program under a small shell that prints
//! these sets as the first line of stdout before it `exec`s the program;
//! [`take_capability_report`] checks and removes that line. The line is
//! written before the program starts, so the program cannot forge it.
//! Both runtimes are checked the same way.
//!
//! [`Container::refusal`]: crate::Container::refusal

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use texrun_process::CapturedOutput;

/// Where a POSIX shell is expected in the image.
const SH: &str = "/bin/sh";

/// Start of the report line.
const MARKER: &[u8] = b"texrun-sandbox-caps";

/// Prints `texrun-sandbox-caps CapInh:<hex> CapPrm:<hex> ...` (the `Cap*`
/// lines of `/proc/self/status`, as the shell itself has them) and `exec`s
/// its arguments, if any.
const REPORT: &str = r#"c=
while read -r k v; do
  case "$k" in Cap*:) c="$c $k$v" ;; esac
done < /proc/self/status
printf 'texrun-sandbox-caps%s\n' "$c"
[ "$#" -eq 0 ] || exec "$@"
"#;

/// The capability sets that must be present in the report.
const REQUIRED: &[&str] = &["CapInh", "CapPrm", "CapEff", "CapBnd"];

/// `program args` run under the reporting shell: the program to start in
/// the container (`/bin/sh`) and its arguments. Without a `program`, the
/// shell only reports.
pub fn capability_report(program: Option<&Path>, args: &[OsString]) -> (PathBuf, Vec<OsString>) {
    let mut shell_args: Vec<OsString> = vec!["-c".into(), REPORT.into(), "sh".into()];
    if let Some(program) = program {
        shell_args.push(program.as_os_str().to_owned());
        shell_args.extend(args.iter().cloned());
    }
    (PathBuf::from(SH), shell_args)
}

/// Checks the report at the start of `stdout` of a [`capability_report`]
/// run and removes it (also from [`CapturedOutput::total_bytes`]), so that
/// what is left is the program's own output. Fails, with the reason, if
/// there is no report or a capability set is not empty.
///
/// A report line is removed even if a set is not empty; nothing is removed
/// without one.
pub fn take_capability_report(stdout: &mut CapturedOutput) -> Result<(), String> {
    let bytes = &mut stdout.bytes;
    if !bytes.starts_with(MARKER) {
        return Err("the container did not report its capabilities".to_owned());
    }
    let Some(end) = bytes.iter().position(|&b| b == b'\n') else {
        return Err("the capability report of the container is incomplete".to_owned());
    };
    let line = String::from_utf8_lossy(&bytes[MARKER.len()..end]).into_owned();
    bytes.drain(..=end);
    stdout.total_bytes = stdout
        .total_bytes
        .saturating_sub(u64::try_from(end + 1).unwrap_or(u64::MAX));
    check_report(&line)
}

/// Checks the fields of a report line (after the marker).
pub(crate) fn check_report(fields: &str) -> Result<(), String> {
    let mut seen = Vec::new();
    let mut held = Vec::new();
    for field in fields.split_whitespace() {
        let parsed = field
            .split_once(':')
            .filter(|(name, _)| name.starts_with("Cap"))
            .and_then(|(name, value)| Some((name, u64::from_str_radix(value, 16).ok()?)));
        let Some((name, value)) = parsed else {
            return Err(format!("unreadable capability report: {field:?}"));
        };
        if value != 0 {
            held.push(format!("{name} {value:016x}"));
        }
        seen.push(name);
    }
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|r| !seen.contains(r))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the capability report lacks {}",
            missing.join(", ")
        ));
    }
    if held.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "a process in the container has capabilities ({})",
            held.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: &str = " CapInh:0000000000000000 CapPrm:0000000000000000 \
                        CapEff:0000000000000000 CapBnd:0000000000000000 \
                        CapAmb:0000000000000000";

    fn captured(text: &str) -> CapturedOutput {
        let mut out = CapturedOutput::default();
        out.bytes = text.as_bytes().to_vec();
        out.total_bytes = out.bytes.len() as u64;
        out
    }

    #[test]
    fn the_program_runs_under_the_reporting_shell() {
        let (program, args) = capability_report(
            Some(Path::new("/usr/bin/latexmk")),
            &["-norc".into(), "-v".into()],
        );
        assert_eq!(program, Path::new("/bin/sh"));
        assert_eq!(
            args,
            [
                OsString::from("-c"),
                REPORT.into(),
                "sh".into(),
                "/usr/bin/latexmk".into(),
                "-norc".into(),
                "-v".into()
            ]
        );
        let (_, alone) = capability_report(None, &[]);
        assert_eq!(alone.len(), 3);
    }

    #[test]
    fn a_report_without_capabilities_passes_and_is_removed() {
        let mut out = captured(&format!(
            "texrun-sandbox-caps{NONE}\nLatexmk, John Collins\n"
        ));
        assert_eq!(take_capability_report(&mut out), Ok(()));
        assert_eq!(out.bytes, b"Latexmk, John Collins\n");
        assert_eq!(out.total_bytes, out.bytes.len() as u64);
        // Kernels before 4.3 have no ambient set.
        let old = NONE.replace(" CapAmb:0000000000000000", "");
        assert_eq!(check_report(&old), Ok(()));
    }

    #[test]
    fn any_capability_fails() {
        for (set, value) in [
            ("CapEff", "0000000000000400"),
            ("CapBnd", "00000000a80425fb"),
            ("CapPrm", "0000000000000001"),
            ("CapInh", "0000000000000001"),
            ("CapAmb", "0000000000002000"),
        ] {
            let fields = NONE.replace(
                &format!("{set}:0000000000000000"),
                &format!("{set}:{value}"),
            );
            let err = check_report(&fields).unwrap_err();
            assert!(err.contains(set), "{set}: {err}");
        }
        // Still removed, so that the error is not mixed into the output.
        let mut out = captured(&format!(
            "texrun-sandbox-caps{}\nx\n",
            NONE.replace("CapBnd:0000000000000000", "CapBnd:00000000a80425fb")
        ));
        assert!(take_capability_report(&mut out).is_err());
        assert_eq!(out.bytes, b"x\n");
    }

    #[test]
    fn a_missing_or_malformed_report_fails() {
        for text in [
            "",
            "Latexmk, John Collins\n",
            "x\ntexrun-sandbox-caps CapEff:0\n",
            "texrun-sandbox-caps CapEff:0000000000000000",
        ] {
            let mut out = captured(text);
            assert!(take_capability_report(&mut out).is_err(), "{text:?}");
        }
        for fields in [
            "",
            " CapEff:0000000000000000",
            &NONE.replace("CapBnd:0000000000000000", "CapBnd:zz"),
            &format!("{NONE} Other:0"),
            &format!("{NONE} CapEff"),
        ] {
            assert!(check_report(fields).is_err(), "{fields:?}");
        }
    }
}
