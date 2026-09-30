//! Running one external tool under the limits of docs/security.md §3.
//!
//! Supervision (cleared environment, own process group killed with
//! `SIGKILL` on timeout / cancellation / an outgrown image and after every
//! exit, kill-before-reap, bounded output, `prlimit(2)` on Linux) is that of
//! `texrun-process`. This module adds the tool-specific values:
//!
//! - the environment allowlist (`PATH`, `HOME`, `LC_ALL`);
//! - the working directory, held as a descriptor ([`ToolEnv::work`]);
//! - on Linux, `RLIMIT_AS` and `RLIMIT_FSIZE` set right after the tool is
//!   spawned ([`TOOL_ADDRESS_SPACE`], [`MIN_FILE_SIZE_LIMIT`]);
//! - a watch on the size of the image being rendered.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;
use texrun_process::{Capture, Cwd, EnvAllowlist, Resource, Rlimits, Spec, Stop, Watch};

use crate::fsops;

/// Kept prefix of stdout (metadata output of up to 200 pages is far smaller).
pub(crate) const STDOUT_LIMIT: usize = 1024 * 1024;
/// Kept prefix of stderr.
pub(crate) const STDERR_LIMIT: usize = 64 * 1024;

/// Address space limit of one tool process on Linux (`RLIMIT_AS`): 2 GiB.
/// A 4096 x 4096 px page needs about 64 MiB of pixels; the rest is room for
/// decoding embedded images. The limit is set right after spawning (there is
/// no `pre_exec` without `unsafe`), so the tool's first instructions run
/// unlimited; the tools do not allocate much before opening the PDF.
pub(crate) const TOOL_ADDRESS_SPACE: u64 = 2 * 1024 * 1024 * 1024;

/// Smallest `RLIMIT_FSIZE` given to a tool on Linux: 16 MiB. The tools may
/// write caches (e.g. fontconfig) into their private `HOME`, and hitting the
/// limit kills them with `SIGXFSZ`, so it is never set below this even when
/// the remaining image budget is smaller; the poll loop enforces the budget.
pub(crate) const MIN_FILE_SIZE_LIMIT: u64 = 16 * 1024 * 1024;

/// The environment of a tool process.
#[derive(Debug)]
pub(crate) struct ToolEnv {
    /// `PATH` for the tool: the absolute entries of the search path used for
    /// detection.
    pub(crate) path: Option<OsString>,
    /// A private, empty directory (the tools may write caches there).
    pub(crate) home: PathBuf,
    /// Working directory; the image is rendered to a relative name in it.
    /// The tool is started in this descriptor itself where possible
    /// ([`Cwd::Dir`]), and the image is inspected and moved through it.
    pub(crate) work: OwnedFd,
    /// A path of [`ToolEnv::work`], for where the descriptor cannot be used.
    pub(crate) work_path: PathBuf,
}

impl ToolEnv {
    fn vars(&self) -> EnvAllowlist {
        let env = EnvAllowlist::new()
            .with("HOME", self.home.as_os_str())
            // Stable, locale-independent number formatting and messages.
            .with("LC_ALL", "C");
        match &self.path {
            Some(path) => env.with_path(path),
            None => env,
        }
    }
}

/// What to watch while the tool runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits<'a> {
    pub(crate) deadline: Instant,
    pub(crate) cancel: &'a CancelToken,
    /// Kill the tool if this file in [`ToolEnv::work`] grows beyond this
    /// many bytes.
    pub(crate) watch: Option<(&'a str, u64)>,
}

/// How the tool run ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    OutputTooLarge,
    /// Spawning, limiting or waiting failed.
    Failed(io::Error),
}

#[derive(Debug)]
pub(crate) struct RunOutput {
    pub(crate) end: RunEnd,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl RunOutput {
    fn empty(end: RunEnd) -> Self {
        Self {
            end,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    pub(crate) fn succeeded(&self) -> bool {
        matches!(&self.end, RunEnd::Exited(status) if status.success())
    }
}

/// Runs `program args...` in `env` and waits for it within `limits`.
pub(crate) fn run(
    program: &Path,
    args: &[OsString],
    env: &ToolEnv,
    capture_stdout: bool,
    limits: Limits<'_>,
) -> RunOutput {
    if limits.cancel.is_cancelled() {
        return RunOutput::empty(RunEnd::Cancelled);
    }
    if Instant::now() >= limits.deadline {
        return RunOutput::empty(RunEnd::TimedOut);
    }

    let file_size = limits
        .watch
        .map_or(0, |(_, budget)| budget.saturating_add(1))
        .max(MIN_FILE_SIZE_LIMIT);
    let cwd = Cwd::Dir {
        fd: env.work.as_fd(),
        path: &env.work_path,
    };
    let spec = Spec::new(program, cwd)
        .with_args(args.iter().cloned())
        .with_env(env.vars())
        .with_stdout(if capture_stdout {
            Capture::Keep(STDOUT_LIMIT)
        } else {
            Capture::Discard
        })
        .with_stderr(Capture::Keep(STDERR_LIMIT))
        .with_rlimits(
            Rlimits::new()
                .with(Resource::AddressSpace, TOOL_ADDRESS_SPACE)
                .with(Resource::FileSize, file_size)
                // `SIGXFSZ` dumps core by default; like the TeX engine,
                // never leave core files behind.
                .with(Resource::Core, 0),
        );

    let mut watch = Watch::new()
        .with_deadline(limits.deadline)
        .with_cancel(limits.cancel);
    if let Some((name, max)) = limits.watch {
        // A single `fstatat`: cheap enough for every poll.
        watch = watch.with_check(Duration::ZERO, move || {
            fsops::regular_file_len(&env.work, name)
                .is_some_and(|len| len > max)
                .then_some(())
        });
    }
    match texrun_process::run(&spec, watch) {
        Ok(done) => RunOutput {
            end: match done.stop {
                None => RunEnd::Exited(done.status),
                Some(Stop::TimedOut) => RunEnd::TimedOut,
                Some(Stop::Cancelled) => RunEnd::Cancelled,
                Some(Stop::Check(())) => RunEnd::OutputTooLarge,
            },
            stdout: done.stdout.bytes,
            stderr: done.stderr.bytes,
        },
        Err(e) => RunOutput::empty(RunEnd::Failed(io::Error::other(e))),
    }
}

/// A short, printable excerpt of tool output for a notice: lossy UTF-8,
/// trimmed, at most `max_chars` characters, control characters (other than
/// newline and tab) escaped so that the text is safe to print.
pub(crate) fn excerpt(bytes: &[u8], max_chars: usize) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i == max_chars {
            out.push_str("...");
            break;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            out.extend(ch.escape_unicode());
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_escapes_control_characters_and_truncates() {
        assert_eq!(excerpt(b"  \n ", 10), None);
        assert_eq!(
            excerpt(b"bad\x1b[31m\nnext", 100).unwrap(),
            "bad\\u{1b}[31m\nnext"
        );
        assert_eq!(excerpt(b"abcdef", 3).unwrap(), "abc...");
        assert_eq!(excerpt(b"\xff ok", 10).unwrap(), "\u{fffd} ok");
    }
}
