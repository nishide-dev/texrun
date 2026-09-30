//! The exec gate (docs/security.md §3.2): a small helper process that sets
//! the resource limits on itself and then `exec`s the program, so that the
//! program runs limited from its first instruction.
//!
//! Setting limits in the child between `fork` and `exec` needs `pre_exec`,
//! i.e. `unsafe`, which the workspace forbids. The gate is a separate
//! program instead ([`run_gate`] is its whole implementation): the
//! supervisor starts the gate, which waits for the start token, calls
//! `setrlimit` on itself and replaces itself with the program. Limits
//! survive `exec`, and the PID (which is also the process group ID) stays
//! the same, so supervision is unchanged. `setrlimit` exists on every Unix
//! system, so this also works on macOS, which has no `prlimit(2)`.
//!
//! # Protocol
//!
//! The command line is
//!
//! ```text
//! <gate> [<gate args>...] texrun-exec-gate/1 [--rlimit <name>=<value>]... -- <program> [<arg>...]
//! ```
//!
//! where `<gate args>` are those of [`ExecGate::with_args`] (e.g. a hidden
//! subcommand) and `<name>` is one of `fsize`, `core`, `as`, `cpu`,
//! `nproc`. Everything after `--` is the program (an absolute path) and its
//! arguments, which the gate passes to `exec` unchanged: the gate never
//! parses or expands them, and never hands them to a shell. (`exec` goes
//! through the C library's `execvp`, which runs a file without a known
//! executable format with `/bin/sh`, exactly as a spawn without the gate
//! would; the programs are executables found by the caller.) The
//! environment and the working directory are the gate's own, i.e. exactly
//! those the supervisor gave it.
//!
//! File descriptors: the program gets stdin (`/dev/null`), stdout and
//! stderr from the supervisor, and none of the gate's own descriptors
//! (they are close-on-exec). A descriptor that is *not* close-on-exec and
//! was inherited by the host process from whoever started it is inherited
//! by the program as well, with or without the gate: closing descriptors
//! the gate does not own needs `unsafe`, which the workspace forbids.
//!
//! stdin of the gate is one end of a Unix socket pair; the supervisor holds
//! the other:
//!
//! 1. the gate checks its arguments, then waits for the start token. The
//!    supervisor sends it after [`Launcher::on_spawn`](crate::Launcher::on_spawn)
//!    (the extension point for #25: moving the gate into a cgroup there
//!    covers the program and every descendant);
//! 2. the gate sets the limits on itself (each capped at its own hard
//!    limit), replaces its stdin by `/dev/null` and reports `ok`;
//! 3. it `exec`s the program. The report channel is close-on-exec, so a
//!    successful `exec` closes it; a failed one is reported (`error exec
//!    <errno>`) and the gate exits.
//!
//! Every failure is reported as `error <stage> <errno or ->` and ends the
//! gate without running the program, with one of the `EXIT_*` statuses of
//! [`ExecGate`]. Further steps before `exec` (e.g. the gate attaching
//! itself to a cgroup, #25) belong between steps 2 and 3, as a new option
//! before `--`.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::rlimit::{self, Resource, Rlimits};

/// First protocol argument: name and version of the protocol.
const PROTOCOL: &str = "texrun-exec-gate/1";
/// What the supervisor writes to release the gate.
pub(crate) const TOKEN: &[u8] = b"texrun-exec-gate: go\n";
/// Longest report kept from the gate (a report is a few short lines).
const MAX_REPORT: usize = 256;

/// Where the exec gate program is, and how to call it.
///
/// There is no default location: a library cannot know where its host
/// binary is. The texrun CLI uses its own executable with a hidden
/// subcommand, and requires it ([`ExecGate::with_required`]): on Linux
/// `ExecGate::new("/proc/self/exe")`, which the child resolves to the image
/// it runs (so it works even after the file was replaced, for children on
/// this host only), elsewhere `std::env::current_exe()`;
/// another program can do the same by calling [`run_gate`] for that
/// subcommand, or use the `texrun-exec-gate` binary of this crate. Nothing
/// is looked up in `PATH` or taken from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecGate {
    program: PathBuf,
    args: Vec<OsString>,
    /// Set by [`ExecGate::unavailable`].
    problem: Option<String>,
    required: bool,
}

impl ExecGate {
    /// Exit status of the gate for invalid arguments.
    pub const EXIT_USAGE: u8 = 2;
    /// Exit status of the gate when stdin ended without the start token.
    pub const EXIT_NOT_RELEASED: u8 = 125;
    /// Exit status of the gate when a limit (or its stdin) could not be set
    /// up.
    pub const EXIT_SETUP: u8 = 126;
    /// Exit status of the gate when `exec` failed (e.g. no such program).
    pub const EXIT_EXEC: u8 = 127;

    /// The gate is the executable `program` (an absolute path), called
    /// without leading arguments.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            problem: None,
            required: false,
        }
    }

    /// A gate that cannot be used, because of `reason` (e.g. the host
    /// binary could not find its own executable). Runs with it fall back or
    /// fail as for any unusable gate ([`ExecGate::check`]), with `reason`
    /// as the explanation.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            problem: Some(reason.into()),
            ..Self::new(PathBuf::new())
        }
    }

    /// Whether a run must fail rather than fall back to
    /// [`StartMode::Immediate`](crate::StartMode::Immediate) when this gate
    /// cannot be used: with `true`, an unusable gate is
    /// [`RunError::Unsupported`](crate::RunError::Unsupported) before
    /// anything is spawned, whatever [`Spec::require_rlimits`](crate::Spec::require_rlimits)
    /// says. Default: `false`.
    ///
    /// Unlike `require_rlimits`, this does not also require every limit to
    /// be settable (macOS cannot set `RLIMIT_AS`, see
    /// [`Resource::AddressSpace`]).
    #[must_use]
    pub fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// See [`ExecGate::with_required`].
    pub fn is_required(&self) -> bool {
        self.required
    }

    /// Arguments placed before the protocol arguments, e.g. the hidden
    /// subcommand that makes a binary run [`run_gate`].
    #[must_use]
    pub fn with_args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The gate executable.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The leading arguments ([`ExecGate::with_args`]).
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Checks that the gate looks usable (an absolute path of an
    /// executable regular file), or says why not. The supervisor does the
    /// same check before every run; a caller can use it to find out once,
    /// up front.
    pub fn check(&self) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(problem) = &self.problem {
            return Err(problem.clone());
        }
        let shown = self.program.display();
        if !self.program.is_absolute() {
            return Err(format!("the exec gate {shown} is not an absolute path"));
        }
        match self.program.metadata() {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => Ok(()),
            Ok(_) => Err(format!("the exec gate {shown} is not an executable file")),
            Err(e) => Err(format!("the exec gate {shown} cannot be used: {e}")),
        }
    }

    /// The arguments of the gate for running `program args...` with
    /// `limits`.
    pub(crate) fn command_args<'s>(
        &self,
        limits: impl Iterator<Item = (Resource, u64)>,
        program: &Path,
        args: impl Iterator<Item = &'s OsString>,
    ) -> Vec<OsString> {
        let mut out = self.args.clone();
        out.push(PROTOCOL.into());
        for (resource, value) in limits {
            out.push("--rlimit".into());
            out.push(format!("{}={value}", rlimit::gate_name(resource)).into());
        }
        out.push("--".into());
        out.push(program.as_os_str().to_owned());
        out.extend(args.cloned());
        out
    }
}

/// The gate side: runs the gate with `args` (the arguments after the
/// executable and the leading [`ExecGate::with_args`] arguments).
///
/// On success this does not return: the process becomes the program. On
/// failure it reports to the supervisor, prints a message to stderr and
/// returns the exit status (one of the `EXIT_*` constants of [`ExecGate`]),
/// without having run anything.
///
/// Call it first thing in `main`, before anything else touches stdin or
/// the resource limits.
///
/// Reports go to stdin only if it is a socket (as set up by the
/// supervisor), so a gate started by hand never writes into a terminal or
/// file.
pub fn run_gate<I>(args: I) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    // The report channel: a close-on-exec copy of stdin, so that a
    // successful `exec` closes it. Only when stdin is a socket (the one the
    // supervisor shares): a gate run by hand must not write into a
    // terminal or a file on its stdin.
    let stdin = rustix::stdio::stdin();
    let report = rustix::fs::fstat(stdin)
        .ok()
        .filter(|st| {
            rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Socket
        })
        .and_then(|_| rustix::io::fcntl_dupfd_cloexec(stdin, 3).ok());
    let fail = |stage: &str, error: Option<&io::Error>, message: String, status: u8| {
        let errno = error.and_then(io::Error::raw_os_error);
        let code = errno.map_or_else(|| "-".to_owned(), |e| e.to_string());
        send(report.as_ref(), &format!("error {stage} {code}\n"));
        let _ = writeln!(io::stderr(), "texrun exec gate: {message}");
        ExitCode::from(status)
    };

    let request = match parse(args) {
        Ok(request) => request,
        Err(message) => return fail("usage", None, message, ExecGate::EXIT_USAGE),
    };
    if let Err(e) = wait_for_token() {
        return fail(
            "token",
            Some(&e),
            format!("start signal missing ({e}); not running"),
            ExecGate::EXIT_NOT_RELEASED,
        );
    }
    for (resource, value) in request.limits.iter() {
        if let Err(e) = rlimit::set_own(resource, value) {
            return fail(
                "rlimit",
                Some(&e),
                format!("cannot set {}: {e}", rlimit::gate_name(resource)),
                ExecGate::EXIT_SETUP,
            );
        }
    }
    // The program gets an empty stdin, as without the gate.
    if let Err(e) = File::open("/dev/null")
        .and_then(|null| rustix::stdio::dup2_stdin(&null).map_err(io::Error::from))
    {
        return fail(
            "stdin",
            Some(&e),
            format!("cannot redirect stdin: {e}"),
            ExecGate::EXIT_SETUP,
        );
    }
    send(report.as_ref(), "ok\n");
    // The environment and the working directory are inherited unchanged.
    let e = Command::new(&request.program).args(&request.args).exec();
    fail(
        "exec",
        Some(&e),
        format!("cannot run {}: {e}", request.program.display()),
        ExecGate::EXIT_EXEC,
    )
}

/// A parsed gate command line.
#[derive(Debug)]
struct Request {
    limits: Rlimits,
    program: PathBuf,
    args: Vec<OsString>,
}

fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Request, String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(OsStr::new(PROTOCOL)) {
        return Err(format!("the first argument must be {PROTOCOL}"));
    }
    let mut limits = Rlimits::new();
    loop {
        let Some(arg) = args.next() else {
            return Err("missing `--` before the program".to_owned());
        };
        if arg == "--" {
            break;
        }
        if arg != "--rlimit" {
            return Err(format!("unexpected argument {}", arg.display()));
        }
        let value = args
            .next()
            .ok_or_else(|| "--rlimit needs a value".to_owned())?;
        let (resource, value) = value
            .to_str()
            .and_then(|v| v.split_once('='))
            .and_then(|(name, value)| {
                let value = value.bytes().all(|b| b.is_ascii_digit()).then_some(value)?;
                Some((rlimit::from_gate_name(name)?, value.parse::<u64>().ok()?))
            })
            .ok_or_else(|| format!("invalid --rlimit value {}", value.display()))?;
        if limits.get(resource).is_some() {
            return Err(format!(
                "--rlimit {} given twice",
                rlimit::gate_name(resource)
            ));
        }
        limits = limits.with(resource, value);
    }
    let program = PathBuf::from(args.next().ok_or_else(|| "missing program".to_owned())?);
    if !program.is_absolute() {
        return Err("the program must be an absolute path".to_owned());
    }
    Ok(Request {
        limits,
        program,
        args: args.collect(),
    })
}

/// Reads stdin until it has [`TOKEN`]. Fails on EOF, a read error or other
/// bytes.
fn wait_for_token() -> io::Result<()> {
    let mut buf = [0u8; TOKEN.len()];
    let mut filled = 0;
    while filled < buf.len() {
        match rustix::io::read(rustix::stdio::stdin(), &mut buf[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    if buf == TOKEN {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "wrong token"))
    }
}

/// Writes `line` to the report channel. Errors are ignored: without a
/// supervisor listening (e.g. a gate run by hand) there is no one to tell.
fn send(report: Option<&OwnedFd>, line: &str) {
    if let Some(fd) = report {
        let _ = rustix::io::write(fd, line.as_bytes());
    }
}

/// What the gate reported, as far as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Report {
    /// Nothing (yet): the gate has not set the limits.
    None,
    /// The limits are set and the gate went on to `exec` (which succeeded
    /// unless an error follows).
    Released,
    /// The gate failed at `stage` and did not run the program.
    Failed { stage: String, errno: Option<i32> },
}

/// The supervisor's end of the gate's stdin.
pub(crate) struct Channel {
    stream: UnixStream,
    buf: Vec<u8>,
    eof: bool,
}

impl Channel {
    /// A socket pair: the supervisor's end and the gate's stdin.
    pub(crate) fn pair() -> io::Result<(Self, OwnedFd)> {
        let (ours, theirs) = UnixStream::pair()?;
        Ok((
            Self {
                stream: ours,
                buf: Vec::new(),
                eof: false,
            },
            theirs.into(),
        ))
    }

    /// Releases the gate. An error means the gate is gone already; its
    /// report (or its absence) says why.
    ///
    /// Never raises `SIGPIPE` (`MSG_NOSIGNAL` on Linux, `SO_NOSIGPIPE` on
    /// macOS), so a host that does not ignore it survives a gate that is
    /// already gone.
    pub(crate) fn release(&mut self) {
        let _ = send_token(&self.stream);
        if self.stream.set_nonblocking(true).is_err() {
            // Without non-blocking reads the channel cannot be polled.
            self.eof = true;
        }
    }

    /// Reads what the gate has reported so far, without blocking.
    pub(crate) fn poll(&mut self) {
        let mut chunk = [0u8; MAX_REPORT];
        while !self.eof {
            match self.stream.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    let room = MAX_REPORT.saturating_sub(self.buf.len());
                    self.buf.extend_from_slice(&chunk[..n.min(room)]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => self.eof = true,
            }
        }
    }

    /// The report read so far.
    pub(crate) fn report(&self) -> Report {
        parse_report(&self.buf)
    }
}

/// Writes [`TOKEN`] to `stream` without raising `SIGPIPE`.
fn send_token(stream: &UnixStream) -> io::Result<()> {
    #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"))]
    rustix::net::sockopt::set_socket_nosigpipe(stream, true)?;
    let mut rest = TOKEN;
    while !rest.is_empty() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = rustix::net::SendFlags::NOSIGNAL;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let flags = rustix::net::SendFlags::empty();
        match rustix::net::send(stream, rest, flags) {
            Ok(n) => rest = &rest[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn parse_report(buf: &[u8]) -> Report {
    let text = String::from_utf8_lossy(buf);
    let mut report = Report::None;
    for line in text.lines() {
        let mut words = line.split(' ');
        match (words.next(), words.next(), words.next()) {
            (Some("ok"), None, None) => report = Report::Released,
            (Some("error"), Some(stage), Some(code)) => {
                return Report::Failed {
                    stage: stage.to_owned(),
                    errno: code.parse::<i32>().ok(),
                };
            }
            _ => {}
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_line_round_trips() {
        let gate = ExecGate::new("/usr/bin/texrun").with_args(["__exec-gate"]);
        let limits = Rlimits::new()
            .with(Resource::FileSize, 1000)
            .with(Resource::Core, 0)
            .with(Resource::AddressSpace, 1 << 31)
            .with(Resource::Cpu, 5)
            .with(Resource::Processes, 7);
        let tool_args = args(&["--", "-x", "a b; c", "$(x)"]);
        let line = gate.command_args(limits.iter(), Path::new("/bin/tool"), tool_args.iter());
        assert_eq!(line[0], "__exec-gate");
        let request = parse(line.into_iter().skip(1)).unwrap();
        assert_eq!(request.limits, limits);
        assert_eq!(request.program, Path::new("/bin/tool"));
        // Arguments after the program are passed on untouched, even `--`.
        assert_eq!(request.args, tool_args);
    }

    #[test]
    fn invalid_command_lines_are_refused() {
        for bad in [
            &[][..],
            &["texrun-exec-gate/2", "--", "/bin/true"],
            &[PROTOCOL, "/bin/true"],
            &[PROTOCOL, "--"],
            &[PROTOCOL, "--", "true"],
            &[PROTOCOL, "--", "./true"],
            &[PROTOCOL, "--rlimit"],
            &[PROTOCOL, "--rlimit", "fsize", "--", "/bin/true"],
            &[PROTOCOL, "--rlimit", "fsize=", "--", "/bin/true"],
            &[PROTOCOL, "--rlimit", "fsize=-1", "--", "/bin/true"],
            &[PROTOCOL, "--rlimit", "fsize=+1", "--", "/bin/true"],
            &[PROTOCOL, "--rlimit", "fsize=1x", "--", "/bin/true"],
            &[PROTOCOL, "--rlimit", "stack=1", "--", "/bin/true"],
            &[
                PROTOCOL,
                "--rlimit",
                "fsize=99999999999999999999",
                "--",
                "/bin/true",
            ],
            &[
                PROTOCOL,
                "--rlimit",
                "core=0",
                "--rlimit",
                "core=1",
                "--",
                "/bin/true",
            ],
            &[PROTOCOL, "--other", "--", "/bin/true"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?}");
        }
        assert!(parse(args(&[PROTOCOL, "--", "/bin/true"])).is_ok());
    }

    #[test]
    fn reports_are_parsed() {
        assert_eq!(parse_report(b""), Report::None);
        assert_eq!(parse_report(b"o"), Report::None);
        assert_eq!(parse_report(b"ok\n"), Report::Released);
        assert_eq!(
            parse_report(b"ok\nerror exec 2\n"),
            Report::Failed {
                stage: "exec".to_owned(),
                errno: Some(2),
            }
        );
        assert_eq!(
            parse_report(b"error usage -\n"),
            Report::Failed {
                stage: "usage".to_owned(),
                errno: None,
            }
        );
    }

    #[test]
    fn a_gate_must_be_an_absolute_executable() {
        assert!(ExecGate::new("sh").check().is_err());
        assert!(ExecGate::new("/nonexistent/texrun-gate").check().is_err());
        assert!(ExecGate::new("/").check().is_err());
        assert_eq!(ExecGate::new("/bin/sh").check(), Ok(()));
        let gone = ExecGate::unavailable("no executable path");
        assert_eq!(gone.check(), Err("no executable path".to_owned()));
        assert!(!gone.is_required());
        assert!(gone.with_required(true).is_required());
    }
}
