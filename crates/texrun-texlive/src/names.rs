//! Character allowlist for names handed to latexmk (docs/security.md §3.5).
//!
//! The texrun-managed rc keeps names away from any shell, so this check is
//! defence in depth: it protects against a gap in the rc or a future latexmk
//! change. It also keeps `"` out of names, which latexmk's `internal` command
//! splitting would otherwise treat as quoting.

use std::path::Path;

use texrun_core::EngineError;

/// Checks one name (a workspace-relative path or an absolute host path)
/// against the allowlist.
///
/// - ASCII: letters, digits, space and `. _ - + , = @ /` only;
/// - non-ASCII: everything except Unicode general category Cf (format
///   characters, which include bidi controls and zero-width characters).
///   Control characters (Cc) are rejected as well.
///
/// No Unicode normalization is applied; the string is judged as given.
pub(crate) fn check_name(what: &str, name: &str) -> Result<(), EngineError> {
    match name.chars().find(|&c| !is_allowed(c)) {
        None => Ok(()),
        Some(c) => Err(EngineError::InvalidRequest(format!(
            "{what} {name:?} contains the character {c:?} (U+{:04X}), which texrun does not \
             pass to latexmk; allowed are letters, digits, non-ASCII text, space and `. _ - + , = @ /`",
            u32::from(c)
        ))),
    }
}

/// [`check_name`] for a host path, which must also be valid UTF-8.
///
/// Called only after the workspace-relative parts passed [`check_name`], so
/// a failure is a property of the host (e.g. the temporary directory name)
/// rather than of the request, and is reported as
/// [`EngineError::Unavailable`].
pub(crate) fn check_host_path(what: &str, path: &Path) -> Result<(), EngineError> {
    let host_error = |detail: String| EngineError::Unavailable {
        engine: crate::ENGINE_NAME.to_owned(),
        reason: format!(
            "{detail}; use a temporary directory whose path texrun accepts (e.g. set TMPDIR)"
        ),
    };
    let Some(text) = path.to_str() else {
        return Err(host_error(format!(
            "{what} {} is not valid UTF-8",
            path.display()
        )));
    };
    check_name(what, text).map_err(|e| match e {
        EngineError::InvalidRequest(detail) => host_error(detail),
        other => other,
    })
}

fn is_allowed(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '.' | '_' | '-' | '+' | ',' | '=' | '@' | '/')
    } else {
        !c.is_control() && !is_format_char(c)
    }
}

/// Unicode general category Cf (format), as of Unicode 16.0.
fn is_format_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> bool {
        check_name("entrypoint", name).is_ok()
    }

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            "main.tex",
            "src/main.tex",
            "my paper.tex",
            "paper-v2_final+rev,1=a@b.tex",
            ".texrun/out",
            "-draft.tex",
            "論文 ドラフト.tex",
            "全角　空白.tex",
            "café.tex",
            "cafe\u{301}.tex",
        ] {
            assert!(ok(name), "{name:?} should be accepted");
        }
    }

    #[test]
    fn rejects_shell_and_tex_specials() {
        for c in [
            '"', '\'', '`', '$', '\\', ';', '|', '&', '<', '>', '(', ')', '[', ']', '{', '}', '*',
            '?', '~', '#', '%', '^', '!', ':', '\t', '\n', '\u{7f}',
        ] {
            let name = format!("a{c}b.tex");
            assert!(!ok(&name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn rejects_format_and_control_characters() {
        for c in [
            '\u{200B}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{E0001}',
            '\u{0085}',
        ] {
            let name = format!("a{c}b.tex");
            assert!(!ok(&name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn error_names_the_character() {
        let err = check_name("entrypoint", "a$b.tex").unwrap_err();
        assert!(matches!(err, EngineError::InvalidRequest(_)));
        assert!(err.to_string().contains("U+0024"), "{err}");
    }

    #[test]
    fn host_paths_are_checked_too() {
        assert!(
            check_host_path(
                "output directory",
                Path::new("/tmp/texrun-ws-1/.texrun/out")
            )
            .is_ok()
        );
        // A bad host path is a host problem, not an invalid request.
        assert!(matches!(
            check_host_path("output directory", Path::new("/tmp/a\"b/out")),
            Err(EngineError::Unavailable { .. })
        ));
    }

    #[test]
    fn unicode_spaces_are_accepted() {
        for c in ['\u{3000}', '\u{00A0}', '\u{2002}', '\u{2028}'] {
            let name = format!("a{c}b.tex");
            assert!(ok(&name), "{name:?} should be accepted");
        }
    }
}
