//! The texrun-managed latexmk rc file (docs/security.md §3.5).
//!
//! latexmk is started with `-norc -r <rc>`, so this file is the only rc it
//! reads. It is written for every compile into a texrun-owned temporary
//! directory outside the workspace, which TeX cannot write to.
//!
//! What it does:
//!
//! - replaces the commands texrun uses (`pdflatex`, `bibtex`, `makeindex`,
//!   and `biber` for later) with `internal texrun_run ...`, a Perl sub that
//!   runs the program with list-form `system { $prog } @args`, so no shell is
//!   involved and nothing in a file name is interpreted;
//! - disables `kpsewhich` (latexmk runs it through a shell) and sets every
//!   other command variable latexmk knows to `NONE` (not implemented);
//! - leaves the hook commands (`$success_cmd`, ...) empty.
//!
//! The rc content is static: no file name, path or other request data is
//! ever interpolated into it.

/// Name of the Perl sub the command variables call.
pub(crate) const RUN_SUB: &str = "texrun_run";

/// Line the parent writes to the child's stdin to let it proceed, when
/// [`RcOptions::stdin_gate`] is set (only used on Linux).
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub(crate) const START_TOKEN: &[u8] = b"texrun-start\n";

/// Exit status of latexmk when the start token did not arrive (spelled out
/// in [`GATE`]; kept here for tests and documentation).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const GATE_EXIT_CODE: i32 = 125;

/// Commands texrun routes through [`RUN_SUB`], as `(variable, command)`.
///
/// `-no-parse-first-line` keeps the first source line from selecting a
/// format; `-no-shell-escape` is repeated here in addition to latexmk's
/// option.
pub(crate) const RUN_COMMANDS: &[(&str, &str)] = &[
    (
        "pdflatex",
        "pdflatex -no-parse-first-line -no-shell-escape %O %S",
    ),
    ("bibtex", "bibtex %O %S"),
    ("makeindex", "makeindex %O -o %D %S"),
    // Not in the MVP image; routed the same way for later use.
    ("biber", "biber %O %S"),
];

/// Every other command variable of latexmk 4.86, set to `NONE`.
///
/// Re-check this list against the latexmk source when latexmk is updated
/// (docs/security.md §3.5).
pub(crate) const NONE_COMMANDS: &[&str] = &[
    // Searching for files: latexmk runs it through a shell (`open "$cmd|"`).
    "kpsewhich",
    // Other TeX engines (MVP uses pdflatex only).
    "latex",
    "xelatex",
    "lualatex",
    "dvilualatex",
    "hilatex",
    // Converters.
    "dvipdf",
    "dvips",
    "dvips_landscape",
    "ps2pdf",
    "xdvipdfmx",
    // Previewers and viewer updates.
    "pdf_previewer",
    "ps_previewer",
    "ps_previewer_landscape",
    "dvi_previewer",
    "dvi_previewer_landscape",
    "hnt_previewer",
    "dvi_update_command",
    "ps_update_command",
    "pdf_update_command",
    // Printing, process listing (`-pvc`), `-use-make`, MS Windows `start`.
    "lpr",
    "lpr_dvi",
    "lpr_pdf",
    "pscmd",
    "make",
    "start_NT",
];

/// Hook and filter variables that must stay empty (run nothing).
pub(crate) const EMPTY_VARIABLES: &[&str] = &[
    "compiling_cmd",
    "success_cmd",
    "warning_cmd",
    "failure_cmd",
    "dvi_filter",
    "ps_filter",
    "pre_tex_code",
];

/// Options for [`render`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RcOptions {
    /// Make latexmk wait, before doing anything else, until the parent has
    /// written [`START_TOKEN`] to its stdin, and exit with
    /// [`GATE_EXIT_CODE`] if stdin ends first. Used on Linux so the parent
    /// can apply `RLIMIT_FSIZE` to latexmk with `prlimit(2)` before latexmk
    /// writes a file or starts a child.
    pub(crate) stdin_gate: bool,
}

/// Waits for [`START_TOKEN`] on stdin (see [`RcOptions::stdin_gate`]).
const GATE: &str = r#"# Wait until texrun has applied resource limits to this process.
{
    my $texrun_token = <STDIN>;
    if (!defined $texrun_token || $texrun_token ne "texrun-start\n") {
        print STDERR "texrun: start signal missing; not running\n";
        exit 125;
    }
}

"#;

/// The [`RUN_SUB`] definition.
///
/// latexmk takes the value returned by a command as a wait status in the
/// encoding of Perl's `system()` (for rules it divides the value by 256 to
/// get the exit code), so the sub returns one:
///
/// - normal exit with code `c`: `c * 256` (0 only for success);
/// - killed by signal `s`: `(128 + s) * 256`;
/// - program could not be started (or no program given): `127 * 256`.
const RUN_SUB_DEFINITION: &str = r#"# Runs a program without a shell. Returns a wait status in the encoding
# of Perl's system(): exit code c -> c * 256; killed by signal s ->
# (128 + s) * 256; could not be started -> 127 * 256.
sub texrun_run {
    my @cmd = @_;
    return 127 * 256 unless @cmd;
    my $prog = $cmd[0];
    my $status = system { $prog } @cmd;
    if ($status == -1) {
        print STDERR "texrun: failed to run '$prog': $!\n";
        return 127 * 256;
    }
    if ($status & 127) {
        return (128 + ($status & 127)) * 256;
    }
    return ($status >> 8) * 256;
}

"#;

/// Renders the rc file.
pub(crate) fn render(options: RcOptions) -> String {
    use std::fmt::Write as _;

    let mut rc = String::from(
        "# Generated by texrun for a single compile. Do not edit.\n\
         # See docs/security.md (section 3.5) in the texrun repository.\n\n",
    );
    if options.stdin_gate {
        rc.push_str(GATE);
    }
    rc.push_str(RUN_SUB_DEFINITION);
    rc.push_str("$pdf_mode = 1;\n$dvi_mode = 0;\n$postscript_mode = 0;\n\n");
    for (var, command) in RUN_COMMANDS {
        let _ = writeln!(rc, "${var} = 'internal {RUN_SUB} {command}';");
    }
    rc.push('\n');
    for var in NONE_COMMANDS {
        let _ = writeln!(rc, "${var} = 'NONE';");
    }
    rc.push('\n');
    for var in EMPTY_VARIABLES {
        let _ = writeln!(rc, "${var} = '';");
    }
    rc.push_str("$print_type = 'none';\n@cus_dep_list = ();\n\n1;\n");
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(rc: &str, var: &str) -> Option<String> {
        let prefix = format!("${var} = ");
        rc.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .map(str::to_owned)
    }

    #[test]
    fn routes_used_commands_through_the_internal_sub() {
        let rc = render(RcOptions::default());
        assert_eq!(
            assignment(&rc, "pdflatex").as_deref(),
            Some("'internal texrun_run pdflatex -no-parse-first-line -no-shell-escape %O %S';")
        );
        assert_eq!(
            assignment(&rc, "bibtex").as_deref(),
            Some("'internal texrun_run bibtex %O %S';")
        );
        assert_eq!(
            assignment(&rc, "makeindex").as_deref(),
            Some("'internal texrun_run makeindex %O -o %D %S';")
        );
        assert_eq!(
            assignment(&rc, "biber").as_deref(),
            Some("'internal texrun_run biber %O %S';")
        );
    }

    #[test]
    fn disables_kpsewhich_and_unused_commands() {
        let rc = render(RcOptions::default());
        for var in [
            "kpsewhich",
            "latex",
            "xelatex",
            "lualatex",
            "dvilualatex",
            "hilatex",
            "dvipdf",
            "dvips",
            "dvips_landscape",
            "ps2pdf",
            "xdvipdfmx",
            "pdf_previewer",
            "ps_previewer",
            "ps_previewer_landscape",
            "dvi_previewer",
            "dvi_previewer_landscape",
            "hnt_previewer",
            "dvi_update_command",
            "ps_update_command",
            "pdf_update_command",
            "lpr",
            "lpr_dvi",
            "lpr_pdf",
            "pscmd",
            "make",
        ] {
            assert_eq!(
                assignment(&rc, var).as_deref(),
                Some("'NONE';"),
                "${var} must be NONE"
            );
        }
        for var in ["success_cmd", "warning_cmd", "failure_cmd", "compiling_cmd"] {
            assert_eq!(assignment(&rc, var).as_deref(), Some("'';"), "${var}");
        }
        assert_eq!(assignment(&rc, "pdf_mode").as_deref(), Some("1;"));
        assert_eq!(assignment(&rc, "dvi_mode").as_deref(), Some("0;"));
        assert_eq!(assignment(&rc, "postscript_mode").as_deref(), Some("0;"));
    }

    #[test]
    fn sub_runs_without_a_shell_and_encodes_the_status() {
        let rc = render(RcOptions::default());
        assert!(rc.contains("sub texrun_run {"));
        // List form with an indirect object: never a shell, even for one
        // argument.
        assert!(rc.contains("system { $prog } @cmd;"));
        assert!(rc.contains("return 127 * 256;"));
        assert!(rc.contains("return (128 + ($status & 127)) * 256;"));
        assert!(rc.contains("return ($status >> 8) * 256;"));
        // No other way of running programs.
        for forbidden in ["exec", "`", "qx", "open(", "open "] {
            assert!(!rc.contains(forbidden), "rc must not contain {forbidden:?}");
        }
        assert!(rc.ends_with("1;\n"));
    }

    #[test]
    fn constants_match_the_perl_text() {
        let rc = render(RcOptions { stdin_gate: true });
        assert!(rc.contains(&format!("sub {RUN_SUB} {{")));
        let token = std::str::from_utf8(START_TOKEN).unwrap().trim_end();
        assert!(rc.contains(&format!("ne \"{token}\\n\"")));
        assert!(rc.contains(&format!("exit {GATE_EXIT_CODE};")));
    }

    #[test]
    fn stdin_gate_is_optional() {
        let plain = render(RcOptions::default());
        assert!(!plain.contains("STDIN"));
        let gated = render(RcOptions { stdin_gate: true });
        assert!(gated.contains("my $texrun_token = <STDIN>;"));
        assert!(gated.contains("$texrun_token ne \"texrun-start\\n\""));
        assert!(gated.contains("exit 125;"));
        // The gate comes before anything else runs.
        assert!(gated.find("<STDIN>").unwrap() < gated.find("sub texrun_run").unwrap());
    }

    #[test]
    fn no_placeholders_other_than_latexmk_ones() {
        // Only latexmk's own `%O` / `%S` / `%D` placeholders appear.
        let rc = render(RcOptions { stdin_gate: true });
        for (i, _) in rc.match_indices('%') {
            let next = rc[i + 1..].chars().next();
            assert!(matches!(next, Some('O' | 'S' | 'D')), "unexpected % at {i}");
        }
    }
}
