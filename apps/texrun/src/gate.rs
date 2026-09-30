//! The exec gate hosted by this binary (docs/security.md §3.2).
//!
//! `texrun __exec-gate ...` runs [`texrun_process::run_gate`]: it sets the
//! resource limits of a preview tool on itself and then `exec`s the tool.
//! The subcommand is internal: `main` dispatches it before parsing the
//! command line, and it is not shown in `--help`.

use texrun_preview::ExecGate;

/// First argument that selects the gate.
pub const SUBCOMMAND: &str = "__exec-gate";

/// The gate: this executable with [`SUBCOMMAND`]. Required: if it cannot
/// be used, previews are skipped with a notice rather than rendered with
/// weaker limits (fail closed).
///
/// On Linux the executable is `/proc/self/exe`, which the spawned child
/// resolves to the image it is running, i.e. this very texrun, even after
/// the file was deleted or replaced (e.g. by a package update while texrun
/// runs). That only holds for a child started on this host, which the
/// preview tools are. Elsewhere it is [`std::env::current_exe`].
pub fn exec_gate() -> ExecGate {
    exe().with_args([SUBCOMMAND]).with_required(true)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn exe() -> ExecGate {
    ExecGate::new("/proc/self/exe")
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn exe() -> ExecGate {
    match std::env::current_exe() {
        Ok(path) => ExecGate::new(path),
        Err(e) => {
            ExecGate::unavailable(format!("the path of the texrun executable is unknown: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_is_this_executable_and_required() {
        let gate = exec_gate();
        if cfg!(target_os = "linux") {
            assert_eq!(gate.program(), std::path::Path::new("/proc/self/exe"));
        } else {
            assert_eq!(gate.program(), std::env::current_exe().unwrap());
        }
        assert_eq!(gate.args(), [SUBCOMMAND]);
        assert!(gate.is_required());
        assert_eq!(gate.check(), Ok(()));
    }
}
