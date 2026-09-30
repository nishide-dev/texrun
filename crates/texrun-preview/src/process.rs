//! Running one external tool under the limits of docs/security.md §3.
//!
//! - started by absolute path with an argv array, never through a shell;
//! - `env_clear()` plus an allowlist (`PATH`, `HOME`, `LC_ALL`);
//! - in its own process group, killed as a group (`SIGKILL`) on timeout,
//!   cancellation or when the watched output file grows past its budget, and
//!   once more after a normal exit to reap stray descendants;
//! - stdout / stderr are drained completely by reader threads, keeping only a
//!   bounded prefix.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use texrun_core::CancelToken;

/// How often the child is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long to wait for the pipe readers after the child is gone. A reader
/// only blocks longer if a descendant escaped the process group and still
/// holds the pipe; its output is then dropped.
const READER_GRACE: Duration = Duration::from_millis(500);
/// Kept prefix of stdout (metadata output of up to 200 pages is far smaller).
pub(crate) const STDOUT_LIMIT: usize = 1024 * 1024;
/// Kept prefix of stderr.
pub(crate) const STDERR_LIMIT: usize = 64 * 1024;

/// The environment of a tool process.
#[derive(Debug, Clone)]
pub(crate) struct ToolEnv {
    /// Host `PATH` (as used for detection).
    pub(crate) path: Option<OsString>,
    /// A private, empty directory (the tools may write caches there).
    pub(crate) home: PathBuf,
    /// Working directory; relative output names are resolved against it.
    pub(crate) cwd: PathBuf,
}

impl ToolEnv {
    fn vars(&self) -> Vec<(&'static str, &OsStr)> {
        let mut vars = vec![
            ("HOME", self.home.as_os_str()),
            // Stable, locale-independent number formatting and messages.
            ("LC_ALL", OsStr::new("C")),
        ];
        if let Some(path) = &self.path {
            vars.push(("PATH", path.as_os_str()));
        }
        vars
    }
}

/// What to watch while the tool runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits<'a> {
    pub(crate) deadline: Instant,
    pub(crate) cancel: &'a CancelToken,
    /// Kill the tool if this file grows beyond this many bytes.
    pub(crate) watch: Option<(&'a Path, u64)>,
}

/// How the tool run ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    OutputTooLarge,
    /// Spawning or waiting failed.
    Failed(io::Error),
}

#[derive(Debug)]
pub(crate) struct RunOutput {
    pub(crate) end: RunEnd,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl RunOutput {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(&self.end, RunEnd::Exited(status) if status.success())
    }
}

/// Runs `program args...` and waits for it within `limits`.
pub(crate) fn run(
    program: &Path,
    args: &[OsString],
    env: &ToolEnv,
    capture_stdout: bool,
    limits: Limits<'_>,
) -> RunOutput {
    let failed = |e| RunOutput {
        end: RunEnd::Failed(e),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    if limits.cancel.is_cancelled() {
        return RunOutput {
            end: RunEnd::Cancelled,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
    }
    if Instant::now() >= limits.deadline {
        return RunOutput {
            end: RunEnd::TimedOut,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env.vars())
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return failed(e),
    };
    let stdout = child.stdout.take().map(|s| drain(s, STDOUT_LIMIT));
    let stderr = child.stderr.take().map(|s| drain(s, STDERR_LIMIT));

    let end = wait(&mut child, limits);
    // Also after a normal exit, so that no descendant outlives the tool. A
    // process group ID is not reused while the group has members, so this
    // cannot reach an unrelated process (docs/security.md §3.6).
    kill_group(&mut child);
    if !matches!(end, RunEnd::Exited(_)) {
        let _ = child.wait();
    }

    let deadline = Instant::now() + READER_GRACE;
    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|rx| {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        })
        .unwrap_or_default()
    };
    RunOutput {
        end,
        stdout: collect(stdout),
        stderr: collect(stderr),
    }
}

fn wait(child: &mut Child, limits: Limits<'_>) -> RunEnd {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return RunEnd::Exited(status),
            Ok(None) => {}
            Err(e) => {
                kill_group(child);
                return RunEnd::Failed(e);
            }
        }
        let stop = if limits.cancel.is_cancelled() {
            Some(RunEnd::Cancelled)
        } else if Instant::now() >= limits.deadline {
            Some(RunEnd::TimedOut)
        } else if limits
            .watch
            .is_some_and(|(path, max)| std::fs::symlink_metadata(path).is_ok_and(|m| m.len() > max))
        {
            Some(RunEnd::OutputTooLarge)
        } else {
            None
        };
        if let Some(end) = stop {
            kill_group(child);
            return end;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn kill_group(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process_group};
    // `ESRCH` (group already gone) is expected and ignored.
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

/// Reads `source` to the end on a thread, keeping at most `limit` bytes.
fn drain(mut source: impl Read + Send + 'static, limit: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match source.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    rx
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
