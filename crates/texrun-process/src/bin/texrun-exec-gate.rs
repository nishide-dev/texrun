//! Standalone exec gate (see `texrun_process::run_gate`), for programs that
//! use `texrun-process` without a binary of their own to host the gate, and
//! for the tests of this crate. The texrun CLI hosts the gate itself
//! (`texrun __exec-gate`).

use std::process::ExitCode;

fn main() -> ExitCode {
    texrun_process::run_gate(std::env::args_os().skip(1))
}
