//! Running latexmk as a supervised process group (docs/security.md §3.2,
//! §3.6).
//!
//! - The child gets its own process group (`process_group(0)`); every stop
//!   and the final cleanup send `SIGKILL` to the whole group, so pdflatex /
//!   bibtex started by latexmk are stopped too.
//! - stdout / stderr are drained by reader threads until EOF; only the first
//!   [`Limits::max_captured_bytes`] of each are kept.
//! - One poll loop checks, in this order, whether the process exited,
//!   whether cancellation was requested, whether the timeout passed and
//!   (every [`Limits::size_check_interval`]) whether the output exceeds its
//!   size limits.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use texrun_core::{CancelToken, EngineError};

use crate::layout;

/// How often the poll loop wakes up.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait for the reader threads after the process group is gone.
/// They normally finish immediately (all writers are dead, so the pipes are
/// at EOF); a process that left the group could keep a pipe open, and is
/// not waited for.
const READER_GRACE: Duration = Duration::from_secs(2);

/// Size limits (docs/security.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum size of one file written by the engine. Enforced with
    /// `RLIMIT_FSIZE` on Linux and, on every platform, by the output size
    /// check.
    pub max_file_bytes: u64,
    /// Maximum total size of the output directory (and the engine's `HOME`).
    pub max_output_bytes: u64,
    /// How much of stdout and of stderr is kept (each). The rest is read and
    /// discarded.
    pub max_captured_bytes: usize,
    /// How often the output size is checked while the engine runs.
    pub size_check_interval: Duration,
}

impl Limits {
    /// Default [`Limits::max_file_bytes`]: 256 MiB.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
    /// Default [`Limits::max_output_bytes`]: 1 GiB.
    pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
    /// Default [`Limits::max_captured_bytes`]: 4 MiB.
    pub const DEFAULT_MAX_CAPTURED_BYTES: usize = 4 * 1024 * 1024;
    /// Default [`Limits::size_check_interval`]: 500 ms.
    pub const DEFAULT_SIZE_CHECK_INTERVAL: Duration = Duration::from_millis(500);

    /// Sets [`Limits::max_file_bytes`].
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Sets [`Limits::max_output_bytes`].
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets [`Limits::max_captured_bytes`].
    #[must_use]
    pub fn with_max_captured_bytes(mut self, bytes: usize) -> Self {
        self.max_captured_bytes = bytes;
        self
    }

    /// Sets [`Limits::size_check_interval`].
    #[must_use]
    pub fn with_size_check_interval(mut self, interval: Duration) -> Self {
        self.size_check_interval = interval;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_output_bytes: Self::DEFAULT_MAX_OUTPUT_BYTES,
            max_captured_bytes: Self::DEFAULT_MAX_CAPTURED_BYTES,
            size_check_interval: Self::DEFAULT_SIZE_CHECK_INTERVAL,
        }
    }
}

/// The first bytes of one output stream of the engine process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapturedOutput {
    /// The kept bytes (at most [`Limits::max_captured_bytes`]).
    pub bytes: Vec<u8>,
    /// How many bytes the process wrote in total.
    pub total_bytes: u64,
}

impl CapturedOutput {
    /// Whether bytes were discarded.
    pub fn is_truncated(&self) -> bool {
        self.total_bytes > self.bytes.len() as u64
    }
}

/// Why the supervisor stopped the process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopReason {
    TimedOut,
    Cancelled,
    /// An output size limit was exceeded; the message says which.
    OutputLimit(String),
}

/// What to watch while the process runs.
pub(crate) struct Watch<'a> {
    pub(crate) timeout: Option<Duration>,
    pub(crate) cancel: Option<&'a CancelToken>,
    /// Directories whose size is limited (empty: no size checks).
    pub(crate) size_dirs: Vec<&'a Path>,
    pub(crate) limits: Limits,
    /// Hold the child at the start gate of the texrun rc (see
    /// [`crate::rc::RcOptions::stdin_gate`]) until `RLIMIT_FSIZE` is set.
    pub(crate) file_size_gate: bool,
}

/// Result of a supervised run.
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) pid: u32,
    pub(crate) status: ExitStatus,
    pub(crate) stop: Option<StopReason>,
    pub(crate) elapsed: Duration,
    pub(crate) stdout: CapturedOutput,
    pub(crate) stderr: CapturedOutput,
}

/// A spawned process group that is killed and reaped when dropped, so no
/// early return (or panic) can leave it running.
struct Group {
    child: Child,
    pgid: Pid,
    reaped: bool,
}

impl Group {
    fn kill(&self) {
        kill_group(self.pgid);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill();
            let _ = self.child.wait();
        }
    }
}

/// Sends `SIGKILL` to the process group. `ESRCH` (nobody left) and other
/// errors are ignored: there is nothing more to do about them.
///
/// Safe to call after the leader was reaped: a process group ID is not
/// reused while any member is alive, so the signal cannot reach an
/// unrelated group.
fn kill_group(pgid: Pid) {
    let _ = kill_process_group(pgid, Signal::KILL);
}

/// Spawns `cmd` (program, args, env and cwd already set) in a new process
/// group and supervises it until it exits or is stopped.
pub(crate) fn run(mut cmd: Command, watch: &Watch<'_>) -> Result<Finished, EngineError> {
    let program = PathBuf::from(cmd.get_program()).display().to_string();
    cmd.stdin(if watch.file_size_gate {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);

    let start = Instant::now();
    let child = cmd
        .spawn()
        .map_err(|source| EngineError::Spawn { program, source })?;
    let leader_pid = child.id();
    let pgid = Pid::from_child(&child);
    let mut group = Group {
        child,
        pgid,
        reaped: false,
    };

    let cap = watch.limits.max_captured_bytes;
    let stdout = spawn_reader(group.child.stdout.take(), cap);
    let stderr = spawn_reader(group.child.stderr.take(), cap);

    if watch.file_size_gate {
        let stdin = group.child.stdin.take();
        release_gate(pgid, stdin, watch.limits.max_file_bytes)?;
    }

    let mut last_size_check = Instant::now();
    let (status, stop) = loop {
        if let Some(status) = group
            .child
            .try_wait()
            .map_err(io_error("waiting for latexmk"))?
        {
            break (status, None);
        }
        let stop = if watch.cancel.is_some_and(CancelToken::is_cancelled) {
            Some(StopReason::Cancelled)
        } else if watch.timeout.is_some_and(|t| start.elapsed() >= t) {
            Some(StopReason::TimedOut)
        } else if !watch.size_dirs.is_empty()
            && last_size_check.elapsed() >= watch.limits.size_check_interval
        {
            last_size_check = Instant::now();
            check_output_size(&watch.size_dirs, &watch.limits)
        } else {
            None
        };
        if let Some(stop) = stop {
            group.kill();
            let status = group
                .child
                .wait()
                .map_err(io_error("waiting for latexmk"))?;
            break (status, Some(stop));
        }
        thread::sleep(POLL_INTERVAL);
    };
    let elapsed = start.elapsed();
    // Clean up whatever latexmk left behind, even after a normal exit.
    group.kill();
    group.reaped = true;

    let deadline = Instant::now() + READER_GRACE;
    let stdout = stdout.finish(deadline);
    let stderr = stderr.finish(deadline);

    Ok(Finished {
        pid: leader_pid,
        status,
        stop,
        elapsed,
        stdout,
        stderr,
    })
}

/// Checks the output size limits once.
pub(crate) fn check_output_size(dirs: &[&Path], limits: &Limits) -> Option<StopReason> {
    let size = layout::tree_size(dirs);
    if size.largest >= limits.max_file_bytes {
        Some(StopReason::OutputLimit(format!(
            "output limit exceeded: a file reached the per-file limit of {} bytes",
            limits.max_file_bytes
        )))
    } else if size.total > limits.max_output_bytes {
        Some(StopReason::OutputLimit(format!(
            "output limit exceeded: the output directory exceeded {} bytes",
            limits.max_output_bytes
        )))
    } else {
        None
    }
}

/// Applies `RLIMIT_FSIZE` to the gated child and lets it proceed.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn release_gate(
    pgid: Pid,
    stdin: Option<std::process::ChildStdin>,
    max_file_bytes: u64,
) -> Result<(), EngineError> {
    use std::io::Write;

    use rustix::process::{Resource, Rlimit, prlimit};

    // `pgid` is the child's PID (it leads its own group).
    let limit = Rlimit {
        current: Some(max_file_bytes),
        maximum: Some(max_file_bytes),
    };
    prlimit(Some(pgid), Resource::Fsize, limit)
        .map_err(|e| io_error("setting RLIMIT_FSIZE on latexmk")(e.into()))?;
    if let Some(mut stdin) = stdin {
        // An error means latexmk is already gone; the poll loop sees its
        // exit status.
        let _ = stdin.write_all(crate::rc::START_TOKEN);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn release_gate(
    _pgid: Pid,
    _stdin: Option<std::process::ChildStdin>,
    _max_file_bytes: u64,
) -> Result<(), EngineError> {
    Err(EngineError::Unsupported(
        "RLIMIT_FSIZE for a child process requires prlimit(2) (Linux)".to_owned(),
    ))
}

/// Whether [`Watch::file_size_gate`] can be used on this platform.
pub(crate) const FILE_SIZE_GATE_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "android"));

fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> EngineError {
    move |source| EngineError::Io {
        context: context.to_owned(),
        source,
    }
}

/// A thread draining one pipe.
struct Reader {
    shared: Arc<Mutex<CapturedOutput>>,
    done: mpsc::Receiver<()>,
}

impl Reader {
    /// Waits until `deadline` for EOF and returns what was captured so far.
    fn finish(self, deadline: Instant) -> CapturedOutput {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    }
}

fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> Reader {
    let shared = Arc::new(Mutex::new(CapturedOutput::default()));
    let (tx, done) = mpsc::channel();
    if let Some(pipe) = pipe {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            drain(pipe, cap, &shared);
            let _ = tx.send(());
        });
    }
    Reader { shared, done }
}

/// Reads `pipe` to EOF, keeping the first `cap` bytes in `out`.
pub(crate) fn drain<R: Read>(mut pipe: R, cap: usize, out: &Mutex<CapturedOutput>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut out = out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        out.total_bytes = out.total_bytes.saturating_add(n as u64);
        let room = cap.saturating_sub(out.bytes.len());
        out.bytes.extend_from_slice(&buf[..n.min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_keeps_the_head_and_counts_everything() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let out = Mutex::new(CapturedOutput::default());
        drain(io::Cursor::new(&data), 100_000, &out);
        let out = out.into_inner().unwrap();
        assert_eq!(out.total_bytes, 200_000);
        assert_eq!(out.bytes, data[..100_000]);
        assert!(out.is_truncated());

        let out = Mutex::new(CapturedOutput::default());
        drain(io::Cursor::new(b"short"), 100, &out);
        let out = out.into_inner().unwrap();
        assert_eq!(out.bytes, b"short");
        assert!(!out.is_truncated());
    }

    fn sleeper(seconds: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        // A shell here is only a test helper standing in for latexmk: it
        // starts a child in the same group and waits for it.
        cmd.args(["-c", &format!("sleep {seconds} & wait")]);
        cmd
    }

    fn watch(timeout: Option<Duration>, cancel: Option<&CancelToken>) -> Watch<'_> {
        Watch {
            timeout,
            cancel,
            size_dirs: Vec::new(),
            limits: Limits::default(),
            file_size_gate: false,
        }
    }

    /// Whether a live (non-zombie) process of group `pgid` exists, waiting
    /// up to 2 s for `SIGKILL` to take effect. Zombies are ignored: in a
    /// container without an init process, killed grandchildren are
    /// reparented to a PID 1 that may never reap them.
    fn group_alive(pgid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let out = Command::new("ps")
                .args(["-A", "-o", "pgid=,stat="])
                .output()
                .expect("ps");
            let alive = String::from_utf8_lossy(&out.stdout).lines().any(|l| {
                let mut f = l.split_whitespace();
                f.next() == Some(&pgid.to_string()) && !f.next().unwrap_or("").starts_with('Z')
            });
            if !alive || Instant::now() >= deadline {
                return alive;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let started = Instant::now();
        let done = run(
            sleeper("30"),
            &watch(Some(Duration::from_millis(300)), None),
        )
        .unwrap();
        assert_eq!(done.stop, Some(StopReason::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!group_alive(done.pid), "grandchild `sleep` must be gone");
    }

    #[test]
    fn cancel_kills_the_whole_group() {
        let cancel = CancelToken::new();
        let c = cancel.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            c.cancel();
        });
        let done = run(sleeper("30"), &watch(None, Some(&cancel))).unwrap();
        t.join().unwrap();
        assert_eq!(done.stop, Some(StopReason::Cancelled));
        assert!(!group_alive(done.pid));
    }

    #[test]
    fn normal_exit_is_reported_and_output_captured() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let done = run(cmd, &watch(Some(Duration::from_secs(20)), None)).unwrap();
        assert_eq!(done.stop, None);
        assert_eq!(done.status.code(), Some(3));
        assert_eq!(done.stdout.bytes, b"out\n");
        assert_eq!(done.stderr.bytes, b"err\n");
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let cmd = Command::new("/nonexistent/texrun-test-program");
        let err = run(cmd, &watch(None, None)).unwrap_err();
        assert!(matches!(err, EngineError::Spawn { .. }), "{err:?}");
    }

    #[test]
    fn size_limits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("b"), vec![0u8; 100]).unwrap();
        let dirs = [dir.path()];
        let limits = Limits::default()
            .with_max_file_bytes(1000)
            .with_max_output_bytes(1000);
        assert_eq!(check_output_size(&dirs, &limits), None);
        assert!(matches!(
            check_output_size(&dirs, &limits.with_max_file_bytes(100)),
            Some(StopReason::OutputLimit(m)) if m.contains("per-file")
        ));
        assert!(matches!(
            check_output_size(&dirs, &limits.with_max_output_bytes(150)),
            Some(StopReason::OutputLimit(m)) if m.contains("output directory")
        ));
    }
}
