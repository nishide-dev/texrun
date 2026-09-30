//! latexmk argv and environment (docs/security.md §3.4, §3.5).

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use texrun_process::EnvAllowlist;
/// The host `PATH` with empty and relative entries removed: latexmk (and the
/// texrun rc) look up `pdflatex` / `bibtex` in `PATH`, and their working
/// directory is inside the workspace (§3.4).
pub(crate) use texrun_process::sanitize_path;

/// `max_print_line` passed to TeX (and to the log parser).
pub(crate) const MAX_PRINT_LINE: usize = 10_000;

/// Builds the latexmk arguments (without the program itself).
///
/// `entry_arg` is the entrypoint file name as a CLI argument (starting with
/// `./`, see [`texrun_core::WorkspacePath::to_cli_arg`]); `output_dir` must be
/// absolute.
pub(crate) fn latexmk_args(rc: &Path, output_dir: &Path, entry_arg: &str) -> Vec<OsString> {
    debug_assert!(output_dir.is_absolute());
    debug_assert!(entry_arg.starts_with("./"));
    let mut outdir = OsString::from("-outdir=");
    outdir.push(output_dir);
    vec![
        "-norc".into(),
        "-r".into(),
        rc.as_os_str().to_owned(),
        "-pdf".into(),
        "-interaction=nonstopmode".into(),
        "-halt-on-error".into(),
        "-file-line-error".into(),
        "-no-shell-escape".into(),
        outdir,
        entry_arg.into(),
    ]
}

/// The complete child environment: the supervisor applies `env_clear()`
/// first, then sets exactly these variables.
///
/// `path` is sanitized again ([`sanitize_path`]). `source_date_epoch` adds
/// `SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE` for reproducible PDFs (§3.9).
pub(crate) fn child_env(path: &OsStr, home: &Path, source_date_epoch: Option<i64>) -> EnvAllowlist {
    let mut env = EnvAllowlist::new()
        .with_path(path)
        .with("HOME", home)
        .with("openin_any", "p")
        .with("openout_any", "p")
        .with("max_print_line", MAX_PRINT_LINE.to_string())
        .with("LC_ALL", "C");
    for name in ["MKTEXTFM", "MKTEXPK", "MKTEXMF", "MKTEXTEX", "MKTEXFMT"] {
        env.set(name, "0");
    }
    if let Some(epoch) = source_date_epoch {
        env.set("SOURCE_DATE_EPOCH", epoch.to_string());
        env.set("FORCE_SOURCE_DATE", "1");
    }
    env
}

/// Finds an executable file called `name` in the (already sanitized) `path`.
pub(crate) fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a regular file (following symlinks) with an execute
/// bit set.
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn argv_matches_the_security_model() {
        let args = latexmk_args(
            Path::new("/tmp/texrun-rc-x/texrun.latexmkrc"),
            Path::new("/tmp/ws/.texrun/out"),
            "./main.tex",
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-norc",
                "-r",
                "/tmp/texrun-rc-x/texrun.latexmkrc",
                "-pdf",
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-file-line-error",
                "-no-shell-escape",
                "-outdir=/tmp/ws/.texrun/out",
                "./main.tex",
            ]
        );
        // The entrypoint is last and cannot be taken for an option.
        assert!(!args.iter().any(|a| *a == "--" || a.starts_with("-e")));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("shell-escape") && !a.starts_with("-no-"))
        );
    }

    #[test]
    fn leading_dash_entrypoint_stays_positional() {
        let args = latexmk_args(Path::new("/rc"), Path::new("/out"), "./-draft.tex");
        assert_eq!(args.last().unwrap(), "./-draft.tex");
    }

    #[test]
    fn env_is_exactly_the_allowlist() {
        let env = child_env(
            OsStr::new("/usr/bin:/bin"),
            Path::new("/tmp/ws/.texrun/home"),
            None,
        );
        let env: BTreeMap<String, String> = env
            .vars()
            .map(|(k, v)| {
                (
                    k.to_str().unwrap().to_owned(),
                    v.to_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/tmp/ws/.texrun/home"),
            ("openin_any", "p"),
            ("openout_any", "p"),
            ("max_print_line", "10000"),
            ("LC_ALL", "C"),
            ("MKTEXTFM", "0"),
            ("MKTEXPK", "0"),
            ("MKTEXMF", "0"),
            ("MKTEXTEX", "0"),
            ("MKTEXFMT", "0"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(env, expected);
        for forbidden in [
            "TEXINPUTS",
            "TEXMFCNF",
            "TEXMFOUTPUT",
            "shell_escape",
            "LATEXMKRCSYS",
        ] {
            assert!(!env.contains_key(forbidden));
        }
    }

    #[test]
    fn source_date_epoch_is_opt_in() {
        let env = child_env(OsStr::new("/bin"), Path::new("/h"), Some(0));
        assert_eq!(env.get("SOURCE_DATE_EPOCH"), Some(OsStr::new("0")));
        assert_eq!(env.get("FORCE_SOURCE_DATE"), Some(OsStr::new("1")));
    }

    #[test]
    fn finds_executables_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("latexmk");
        std::fs::write(&exe, "#!/bin/false\n").unwrap();
        let path = dir.path().as_os_str();
        assert_eq!(find_in_path("latexmk", path), None, "not executable yet");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in_path("latexmk", path), Some(exe));
        std::fs::create_dir(dir.path().join("dir")).unwrap();
        assert_eq!(find_in_path("dir", path), None);
    }
}
