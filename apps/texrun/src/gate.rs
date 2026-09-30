//! The exec gate hosted by this binary (docs/security.md §3.2).
//!
//! `texrun __exec-gate ...` runs [`texrun_process::run_gate`]: it sets the
//! resource limits of a preview tool on itself and then `exec`s the tool.
//! The subcommand is internal: `main` dispatches it before parsing the
//! command line, and it is not shown in `--help`.

use texrun_preview::ExecGate;

/// First argument that selects the gate.
pub const SUBCOMMAND: &str = "__exec-gate";

/// The gate: this executable with [`SUBCOMMAND`]. `None` if the path of
/// this executable is unknown; the preview tools then get their limits
/// after they started (best effort, see `texrun_preview::Previewer`).
pub fn exec_gate() -> Option<ExecGate> {
    let exe = std::env::current_exe().ok()?;
    Some(ExecGate::new(exe).with_args([SUBCOMMAND]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_is_this_executable() {
        let gate = exec_gate().unwrap();
        assert_eq!(gate.program(), std::env::current_exe().unwrap());
        assert_eq!(gate.args(), [SUBCOMMAND]);
    }
}
