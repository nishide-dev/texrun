//! Who created a container or a scratch directory, so that the ones a
//! killed texrun left behind can be reclaimed by a later one (#49,
//! docs/security.md §4 "container のライフサイクル").
//!
//! A [`Creator`] names a texrun process by its PID, the start time of that
//! process (where the kernel tells it without `unsafe`: Linux) and the
//! "host" its PID belongs to (a hash of what makes PIDs comparable: the
//! boot and the PID namespace on Linux, the host name elsewhere). A later
//! texrun considers the creator gone only if it is on the same host and
//! no process has that PID any more, or one with another start time has
//! (the PID was reused). Whenever it cannot tell, the creator counts as
//! alive: a leftover is kept rather than something in use removed.

use std::sync::OnceLock;

use rustix::io::Errno;
use rustix::process::Pid;

/// A texrun process that created a container or a scratch directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Creator {
    pid: u32,
    started: Option<u64>,
    host: u64,
}

impl Creator {
    /// This texrun process; `None` if the host cannot be identified (then
    /// nothing it creates is ever reclaimed).
    pub fn current() -> Option<Self> {
        static CURRENT: OnceLock<Option<Creator>> = OnceLock::new();
        *CURRENT.get_or_init(|| {
            let pid = std::process::id();
            Some(Self {
                pid,
                started: start_time(pid),
                host: host_id()?,
            })
        })
    }

    /// A creator from its parts (as recorded in labels).
    pub fn new(pid: u32, started: Option<u64>, host: u64) -> Self {
        Self { pid, started, host }
    }

    /// The PID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The start time of the process (Linux: clock ticks since boot, from
    /// `/proc/<pid>/stat`); `None` where it is not known.
    pub fn started(&self) -> Option<u64> {
        self.started
    }

    /// The hash of the host the PID belongs to.
    pub fn host(&self) -> u64 {
        self.host
    }

    /// A short text form for names: `<pid>-<started or x>-<host>` (decimal,
    /// decimal, 16 hex digits; only `[0-9a-fx-]`).
    pub fn tag(&self) -> String {
        let started = self
            .started
            .map_or_else(|| "x".to_owned(), |s| s.to_string());
        format!("{}-{started}-{:016x}", self.pid, self.host)
    }

    /// Parses [`Creator::tag`] at the start of `text`; returns the creator
    /// and the rest of `text` after the tag.
    pub fn parse_tag(text: &str) -> Option<(Self, &str)> {
        let mut parts = text.splitn(4, '-');
        let pid = parse_decimal(parts.next()?)?;
        let started = match parts.next()? {
            "x" => None,
            s => Some(parse_decimal(s)?),
        };
        let host = parts.next()?;
        if host.len() != 16 || !host.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let host = u64::from_str_radix(host, 16).ok()?;
        let rest = parts.next().unwrap_or("");
        Some((Self::new(u32::try_from(pid).ok()?, started, host), rest))
    }

    /// Whether this creator is known to be gone: it is on this host (as
    /// [`Creator::current`]) and its process no longer exists, or the PID
    /// now belongs to a process with another start time. `false` whenever
    /// that cannot be told.
    pub fn is_gone(&self) -> bool {
        let Some(me) = Self::current() else {
            return false;
        };
        if self.host != me.host || self.pid == me.pid {
            return false;
        }
        let Some(pid) = i32::try_from(self.pid).ok().and_then(Pid::from_raw) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Err(Errno::SRCH) => true,
            // Some process has the PID (`EPERM`: another user's).
            Ok(()) | Err(Errno::PERM) => match (self.started, start_time(self.pid)) {
                (Some(recorded), Some(now)) => recorded != now,
                _ => false,
            },
            Err(_) => false,
        }
    }
}

fn parse_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The start time of process `pid` in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). Linux only.
fn start_time(pid: u32) -> Option<u64> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is in parentheses and may contain spaces
    // and parentheses itself: count from the last `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts with field 3 (the state).
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// What makes PIDs of this process comparable with recorded ones, hashed.
///
/// - Linux: the boot (`/proc/sys/kernel/random/boot_id`) and the PID
///   namespace (`/proc/self/ns/pid`): a texrun in another container sees
///   other PIDs, even with the same runtime socket.
/// - Elsewhere (macOS: no PID namespaces): the OS and the host name. The
///   runtime is always a local one (a `unix://` socket).
fn host_id() -> Option<u64> {
    let text = if cfg!(any(target_os = "linux", target_os = "android")) {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let ns = std::fs::read_link("/proc/self/ns/pid").ok()?;
        format!("linux\n{}\n{}", boot.trim(), ns.display())
    } else {
        let uname = rustix::system::uname();
        let node = uname.nodename().to_string_lossy();
        if node.is_empty() {
            return None;
        }
        format!("{}\n{node}", std::env::consts::OS)
    };
    Some(fnv1a(text.as_bytes()))
}

/// 64-bit FNV-1a: stable across texrun versions and Rust releases (unlike
/// the standard library's hashers).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        let c = Creator::new(1234, Some(98765), 0x0123_4567_89ab_cdef);
        assert_eq!(c.tag(), "1234-98765-0123456789abcdef");
        assert_eq!(
            Creator::parse_tag("1234-98765-0123456789abcdef-rest-x"),
            Some((c, "rest-x"))
        );
        let c = Creator::new(7, None, 1);
        assert_eq!(c.tag(), "7-x-0000000000000001");
        assert_eq!(Creator::parse_tag(&c.tag()), Some((c, "")));
        for bad in [
            "",
            "x-1-0123456789abcdef",
            "1-1-0123",
            "1-1-0123456789abcdeg",
            "-1-1-0123456789abcdef",
            "99999999999-1-0123456789abcdef",
            "1--0123456789abcdef",
        ] {
            assert_eq!(Creator::parse_tag(bad), None, "{bad}");
        }
    }

    #[test]
    fn this_process_is_not_gone() {
        let me = Creator::current().expect("the host can be identified");
        assert_eq!(me.pid(), std::process::id());
        assert!(!me.is_gone());
        if cfg!(target_os = "linux") {
            assert!(me.started().is_some());
        }
    }

    #[test]
    fn an_exited_process_is_gone_only_on_the_same_host() {
        let me = Creator::current().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let gone = Creator::new(pid, None, me.host());
        assert!(gone.is_gone());
        // Another host: never.
        assert!(!Creator::new(pid, None, me.host() ^ 1).is_gone());
    }

    #[test]
    fn a_reused_pid_is_told_apart_by_its_start_time() {
        let me = Creator::current().unwrap();
        // The parent of the test (alive): the same PID with another start
        // time is a reused PID where start times are known, and alive
        // elsewhere.
        let parent = rustix::process::getppid().map_or(1, |p| p.as_raw_nonzero().get());
        let parent = u32::try_from(parent).unwrap();
        let reused = Creator::new(parent, Some(u64::MAX), me.host());
        assert_eq!(reused.is_gone(), cfg!(target_os = "linux"));
        let unknown = Creator::new(parent, None, me.host());
        assert!(!unknown.is_gone());
        if let Some(started) = start_time(parent) {
            assert!(!Creator::new(parent, Some(started), me.host()).is_gone());
        }
    }

    #[test]
    fn the_hash_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
