//! Reclaiming containers that a killed texrun left behind (#49, #56).
//!
//! Every container texrun creates carries, besides [`LABEL`](crate::LABEL),
//! labels naming its creator ([`Creator`]): its PID, the start time of that
//! process (Linux), the host the PID belongs to, the machine and the boot
//! (where known), and the effective uid of texrun.
//! [`Runtime::reclaim_left_containers`] removes a container only if all of
//! these hold:
//!
//! - it has the labels of the PID, the uid and the host, with the uid of
//!   this texrun, and the host of this texrun or (#56) the machine of this
//!   texrun;
//! - its name is the one texrun gives a container of that PID
//!   (`texrun-<pid>-...`);
//! - its creator is gone ([`Creator::is_gone`]): on the same host (or, on
//!   macOS, the same machine), no process has the PID any more, or one
//!   with another start time (the PID was reused); or (#56, Linux) it ran
//!   on this machine in another boot and the container was created before
//!   this boot;
//! - it is not running: exited (or dead), or created but never started.
//!
//! It is removed with `rm` without `--force`, so a container that started
//! running in the meantime is refused by the runtime and kept.
//!
//! Labels do not authenticate anything: whoever can create containers on
//! the daemon can set them. That is the daemon's owner (root-equivalent
//! with Docker) or, with rootless Podman, the user's own store, who can
//! remove these containers anyway. The checks make sure that texrun only
//! removes stopped containers of texrun processes of the same user that
//! are known to be gone, never one in use; a container with forged labels
//! that pass them is, at worst, removed after it stopped.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::{Duration, SystemTime};

use crate::error::SandboxError;
use crate::owner::{Creator, Relation, parse_hex16, relation};
use crate::runtime::Runtime;

/// Label with the PID of the texrun process that created the container.
pub const LABEL_PID: &str = "org.texrun.sandbox.pid";
/// Label with the start time of that process ([`Creator::started`]).
pub const LABEL_STARTED: &str = "org.texrun.sandbox.started";
/// Label with the host of that PID ([`Creator::host`], 16 hex digits).
pub const LABEL_HOST: &str = "org.texrun.sandbox.host";
/// Label with the effective uid of that process.
pub const LABEL_UID: &str = "org.texrun.sandbox.uid";
/// Label with the machine of that process ([`Creator::machine`], 16 hex
/// digits; only where it is known).
pub const LABEL_MACHINE: &str = "org.texrun.sandbox.machine";
/// Label with the boot of that process ([`Creator::boot`], 16 hex digits;
/// only where it is known).
pub const LABEL_BOOT: &str = "org.texrun.sandbox.boot";

/// Timeout of `rm` (without `--force`) of a left container.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// At most this many containers are inspected with one command.
const INSPECT_CHUNK: usize = 64;

/// The `--label` values of a container created by `creator` (or by an
/// unidentified host: then only the PID and uid, and it is never
/// reclaimed).
pub(crate) fn creator_labels(creator: Option<Creator>) -> Vec<String> {
    let mut labels = vec![
        format!("{}=1", crate::LABEL),
        format!("{LABEL_PID}={}", std::process::id()),
        format!("{LABEL_UID}={}", rustix::process::geteuid().as_raw()),
    ];
    if let Some(creator) = creator {
        if let Some(started) = creator.started() {
            labels.push(format!("{LABEL_STARTED}={started}"));
        }
        labels.push(format!("{LABEL_HOST}={:016x}", creator.host()));
        if let Some(machine) = creator.machine() {
            labels.push(format!("{LABEL_MACHINE}={machine:016x}"));
        }
        if let Some(boot) = creator.boot() {
            labels.push(format!("{LABEL_BOOT}={boot:016x}"));
        }
    }
    labels
}

/// One container as `inspect` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    /// When the runtime created it (`None` if that cannot be read).
    pub(crate) created: Option<SystemTime>,
    pub(crate) labels: BTreeMap<String, String>,
}

/// The creator of a container that may be reclaimed by `me` (euid `uid`),
/// from its labels, name and state; `None` if it must be kept whatever
/// the creator's state.
pub(crate) fn candidate(listed: &Listed, me: &Creator, uid: u32) -> Option<Creator> {
    let label = |key: &str| listed.labels.get(key).map(String::as_str);
    if label(crate::LABEL) != Some("1") || label(LABEL_UID) != Some(&uid.to_string()) {
        return None;
    }
    let host = u64::from_str_radix(label(LABEL_HOST)?, 16).ok()?;
    let pid: u32 = label(LABEL_PID)?.parse().ok()?;
    let started = match label(LABEL_STARTED) {
        Some(s) => Some(s.parse().ok()?),
        None => None,
    };
    // A malformed machine or boot keeps the container, like a malformed
    // start time. Absent ones (#49) leave only the host to compare.
    let id = |key: &str| match label(key) {
        Some(id) => parse_hex16(id).map(Some),
        None => Some(None),
    };
    let creator =
        Creator::new(pid, started, host).with_machine(id(LABEL_MACHINE)?, id(LABEL_BOOT)?);
    if relation(&creator, me) == Relation::Unknown {
        return None;
    }
    // Docker reports `/name`, Podman `name`.
    let name = listed.name.strip_prefix('/').unwrap_or(&listed.name);
    if !name.starts_with(&format!("texrun-{pid}-")) {
        return None;
    }
    // Not running: exited / dead (Docker), exited / stopped (Podman), or
    // created and never started (`created`, Podman also `configured`).
    let stopped = matches!(
        listed.status.as_str(),
        "exited" | "dead" | "stopped" | "created" | "configured"
    );
    stopped.then_some(creator)
}

/// The `--format` of `inspect` that [`parse_listing`] reads. `.Created` is
/// a string with Docker and a time with Podman; as JSON both are RFC 3339.
const LISTING_FORMAT: &str =
    "{{.Id}} {{.Name}} {{.State.Status}} {{json .Created}} {{json .Config.Labels}}";

/// Parses `inspect --format LISTING_FORMAT`, one container per line.
pub(crate) fn parse_listing(out: &str) -> Vec<Listed> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.trim().splitn(5, ' ');
            let id = fields.next()?.to_owned();
            let name = fields.next()?.to_owned();
            let status = fields.next()?.to_owned();
            let created = serde_json::from_str::<String>(fields.next()?)
                .ok()
                .and_then(|time| parse_rfc3339(&time));
            let labels: BTreeMap<String, String> =
                serde_json::from_str::<Option<_>>(fields.next()?)
                    .ok()?
                    .unwrap_or_default();
            (!id.is_empty()).then_some(Listed {
                id,
                name,
                status,
                created,
                labels,
            })
        })
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)` as a time; `None` for
/// anything else, or before 1970.
pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    fn number(s: &str, len: usize) -> Option<i64> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    }
    let (date, rest) = text.split_once(['T', 't'])?;
    let mut date = date.splitn(3, '-');
    let year = number(date.next()?, 4)?;
    let month = number(date.next()?, 2)?;
    let day = number(date.next()?, 2)?;
    let (time, zone) = rest.split_at(rest.find(['Z', 'z', '+', '-'])?);
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, Some(fraction)),
        None => (time, None),
    };
    let mut time = time.splitn(3, ':');
    let hour = number(time.next()?, 2)?;
    let minute = number(time.next()?, 2)?;
    let second = number(time.next()?, 2)?;
    let nanos: u32 = match fraction {
        None => 0,
        Some(fraction) => {
            if !(1..=32).contains(&fraction.len()) || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let digits = &fraction[..fraction.len().min(9)];
            format!("{digits:0<9}").parse().ok()?
        }
    };
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let (sign, hours_minutes) = zone.split_at(1);
            let (hours, minutes) = hours_minutes.split_once(':')?;
            let (hours, minutes) = (number(hours, 2)?, number(minutes, 2)?);
            if hours >= 24 || minutes >= 60 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if sign == "-" { -offset } else { offset }
        }
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour >= 24
        || minute >= 60
        || second >= 61
    {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let seconds = u64::try_from(seconds).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

/// Days from 1970-01-01 to the given date of the proleptic Gregorian
/// calendar (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

impl Runtime {
    /// Removes the containers that texrun processes of this user on this
    /// host (or, #56, on this machine before it was rebooted or renamed)
    /// created and left behind when they were killed (see the module
    /// documentation for the exact conditions); returns how many were
    /// removed. Running containers and those of live texrun processes are
    /// never touched. A container that cannot be removed is skipped.
    ///
    /// Fails only if the containers cannot be listed.
    pub fn reclaim_left_containers(&self) -> Result<usize, SandboxError> {
        let Some(me) = Creator::current() else {
            return Ok(0);
        };
        let uid = rustix::process::geteuid().as_raw();
        // Not filtered by the host: one of an earlier boot (or host name)
        // has another. `candidate` checks the host or the machine.
        let filters = [
            format!("label={}=1", crate::LABEL),
            format!("label={LABEL_UID}={uid}"),
        ];
        let mut args = vec!["ps", "--all", "--quiet", "--no-trunc"];
        for filter in &filters {
            args.extend(["--filter", filter]);
        }
        let ids: Vec<String> = self
            .query(&args)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut removed = 0;
        for chunk in ids.chunks(INSPECT_CHUNK) {
            for listed in self.inspect_listing(chunk) {
                let Some(creator) = candidate(&listed, &me, uid) else {
                    continue;
                };
                if !creator.is_gone(listed.created) {
                    continue;
                }
                // Without `--force`: a running container is refused.
                let rm: Vec<OsString> = ["rm", "--", listed.id.as_str()]
                    .into_iter()
                    .map(OsString::from)
                    .collect();
                if self.exec(&rm, REMOVE_TIMEOUT).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// The containers `ids` as `inspect` describes them. A container
    /// removed since it was listed (e.g. by another texrun that just
    /// finished) fails the whole command, so then each one is inspected on
    /// its own and the missing ones are left out.
    fn inspect_listing(&self, ids: &[String]) -> Vec<Listed> {
        let inspect = |ids: &[String]| {
            let mut args: Vec<&str> = vec![
                "inspect",
                "--type",
                "container",
                "--format",
                LISTING_FORMAT,
                "--",
            ];
            args.extend(ids.iter().map(String::as_str));
            self.query(&args).map(|out| parse_listing(&out))
        };
        match inspect(ids) {
            Ok(listed) => listed,
            Err(_) if ids.len() > 1 => ids
                .chunks(1)
                .filter_map(|id| inspect(id).ok())
                .flatten()
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Creator {
        Creator::new(100, Some(5), 0xabc)
    }

    fn listed(name: &str, status: &str, labels: &[(&str, &str)]) -> Listed {
        Listed {
            id: "0123".to_owned(),
            name: name.to_owned(),
            status: status.to_owned(),
            created: None,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    const HOST: &str = "0000000000000abc";

    fn full(pid: &str) -> Vec<(&'static str, String)> {
        vec![
            (crate::LABEL, "1".to_owned()),
            (LABEL_PID, pid.to_owned()),
            (LABEL_STARTED, "77".to_owned()),
            (LABEL_HOST, HOST.to_owned()),
            (LABEL_UID, "501".to_owned()),
        ]
    }

    fn with<'a>(labels: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
        labels.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    #[test]
    fn only_stopped_containers_of_this_user_and_host_are_candidates() {
        let labels = full("42");
        let ok = listed("/texrun-42-0-000000001", "exited", &with(&labels));
        assert_eq!(
            candidate(&ok, &me(), 501),
            Some(Creator::new(42, Some(77), 0xabc))
        );
        // Podman's name and states.
        for status in ["exited", "stopped", "created", "configured", "dead"] {
            let l = listed("texrun-42-3-1", status, &with(&labels));
            assert!(candidate(&l, &me(), 501).is_some(), "{status}");
        }
        for status in ["running", "paused", "restarting", "removing", ""] {
            let l = listed("texrun-42-3-1", status, &with(&labels));
            assert_eq!(candidate(&l, &me(), 501), None, "{status}");
        }
        // Another user, another host.
        assert_eq!(candidate(&ok, &me(), 502), None);
        assert_eq!(candidate(&ok, &Creator::new(100, None, 0xabd), 501), None);
        // A name that is not texrun's for that PID.
        for name in [
            "/texrun-43-0-1",
            "/other",
            "/texrun-420-0-1",
            "/xtexrun-42-0",
        ] {
            let l = listed(name, "exited", &with(&labels));
            assert_eq!(candidate(&l, &me(), 501), None, "{name}");
        }
        // A missing or malformed label.
        for drop in [crate::LABEL, LABEL_PID, LABEL_HOST, LABEL_UID] {
            let some: Vec<_> = labels.iter().filter(|(k, _)| *k != drop).cloned().collect();
            let l = listed("/texrun-42-0-1", "exited", &with(&some));
            assert_eq!(candidate(&l, &me(), 501), None, "without {drop}");
        }
        let mut bad = labels.clone();
        bad[2].1 = "soon".to_owned();
        let l = listed("/texrun-42-0-1", "exited", &with(&bad));
        assert_eq!(candidate(&l, &me(), 501), None);
        // Without a start time (not Linux): still a candidate.
        let no_start: Vec<_> = labels
            .iter()
            .filter(|(k, _)| *k != LABEL_STARTED)
            .cloned()
            .collect();
        let l = listed("/texrun-42-0-1", "exited", &with(&no_start));
        assert_eq!(
            candidate(&l, &me(), 501),
            Some(Creator::new(42, None, 0xabc))
        );
    }

    /// A container that disappears between `ps` and `inspect` (removed by
    /// another texrun) does not keep the others from being reclaimed.
    #[test]
    fn a_container_removed_meanwhile_does_not_hide_the_others() {
        use std::os::unix::fs::PermissionsExt;

        let me = Creator::current().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let labels = serde_json::json!({
            crate::LABEL: "1",
            LABEL_PID: dead.to_string(),
            LABEL_UID: uid.to_string(),
            LABEL_HOST: format!("{:016x}", me.host()),
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("left"),
            format!("left-id /texrun-{dead}-0-0 exited \"2026-01-02T03:04:05.6Z\" {labels}\n"),
        )
        .unwrap();
        let script = dir.path().join("docker");
        std::fs::write(
            &script,
            r#"#!/bin/sh
here=$(dirname "$0")
echo "$*" >> "$here/calls"
case "$1" in
  version) echo "29.0.0 linux" ;;
  context) echo "unix:///var/run/docker.sock" ;;
  ps) echo gone-id; echo left-id ;;
  inspect)
    case "$*" in *gone-id*) echo "No such object" >&2; exit 1 ;; esac
    cat "$here/left" ;;
  rm) ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rt = loop {
            match Runtime::with_program(crate::RuntimeKind::Docker, script.clone()) {
                Ok(rt) => break rt,
                Err(e) if e.to_string().contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(rt.reclaim_left_containers().unwrap(), 1);
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.contains("rm -- left-id"), "{calls}");
        assert!(!calls.contains("rm -- gone-id"), "{calls}");
    }

    #[test]
    fn inspect_output_is_parsed() {
        let out = "abc /texrun-1-0-1 exited \"1970-01-01T00:00:10.5Z\" \
                   {\"org.texrun.sandbox\":\"1\",\"x\":\"a b\"}\n\
                   def name running \"soon\" null\n\
                   broken\n";
        let parsed = parse_listing(out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "abc");
        assert_eq!(parsed[0].name, "/texrun-1-0-1");
        assert_eq!(parsed[0].status, "exited");
        assert_eq!(
            parsed[0].created,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(10_500))
        );
        assert_eq!(parsed[0].labels["x"], "a b");
        // A creation time that cannot be read is not known.
        assert_eq!(parsed[1].created, None);
        assert!(parsed[1].labels.is_empty());
    }

    #[test]
    fn creation_times_are_parsed() {
        let at = |secs: u64, nanos: u32| Some(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos));
        // `date -u -d 2026-08-29T09:57:58Z +%s`
        let docker = 1_787_997_478;
        for (text, expected) in [
            ("1970-01-01T00:00:00Z", at(0, 0)),
            ("2026-08-29T09:57:58.283401317Z", at(docker, 283_401_317)),
            // Podman: the local offset.
            (
                "2026-08-29T18:57:58.283401317+09:00",
                at(docker, 283_401_317),
            ),
            ("2026-08-29T05:27:58-04:30", at(docker, 0)),
            ("2000-02-29T00:00:00Z", at(951_782_400, 0)),
            (
                "2024-12-31T23:59:59.1234567891234Z",
                at(1_735_689_599, 123_456_789),
            ),
        ] {
            assert_eq!(parse_rfc3339(text), expected, "{text}");
        }
        for bad in [
            "",
            "0001-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "2026-08-29 09:57:58Z",
            "2026-08-29T09:57:58",
            "2026-13-29T09:57:58Z",
            "2026-08-29T24:57:58Z",
            "2026-08-29T09:57:58.Z",
            "2026-08-29T09:57:58.x1Z",
            "2026-08-29T09:57:58+0900",
            "2026-8-29T09:57:58Z",
            "+2026-08-29T09:57:58Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    /// #56: the labels of the machine and the boot. A container of this
    /// machine is a candidate even with another host; one without these
    /// labels (#49) only with this host.
    #[test]
    fn containers_of_this_machine_are_candidates_whatever_the_host() {
        let me = Creator::new(100, Some(5), 0xabc).with_machine(Some(0xd), Some(0xb0));
        let other_host = |extra: &[(&'static str, &'static str)]| {
            let mut labels = full("42");
            labels[3].1 = "0000000000000abd".to_owned();
            labels.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
            labels
        };
        let machine = (LABEL_MACHINE, "000000000000000d");
        let other_machine = (LABEL_MACHINE, "000000000000000e");
        let boot = |b: &'static str| (LABEL_BOOT, b);
        let candidate_of = |labels: &[(&'static str, String)]| {
            candidate(&listed("/texrun-42-0-1", "exited", &with(labels)), &me, 501)
        };
        // Another boot of this machine.
        assert_eq!(
            candidate_of(&other_host(&[machine, boot("00000000000000b1")])),
            Some(Creator::new(42, Some(77), 0xabd).with_machine(Some(0xd), Some(0xb1)))
        );
        // Kept: another machine, no machine or boot (#49), a malformed one.
        for extra in [
            vec![other_machine, boot("00000000000000b1")],
            vec![],
            vec![boot("00000000000000b1")],
            vec![machine],
            vec![(LABEL_MACHINE, "d"), boot("00000000000000b1")],
            vec![machine, boot("b1")],
        ] {
            assert_eq!(candidate_of(&other_host(&extra)), None, "{extra:?}");
        }
        // This host: a malformed machine label still keeps it.
        let mut labels = full("42");
        labels.push((LABEL_MACHINE, "zz".to_owned()));
        assert_eq!(candidate_of(&labels), None);
        // This host, with or without the new labels.
        let mut labels = full("42");
        labels.push((LABEL_MACHINE, "000000000000000e".to_owned()));
        assert!(candidate_of(&labels).is_some());
        assert!(candidate_of(&full("42")).is_some());
    }

    #[test]
    fn labels_name_the_creator() {
        let creator = Creator::new(std::process::id(), Some(9), 0x1f);
        let labels = creator_labels(Some(creator));
        assert!(labels.contains(&format!("{}=1", crate::LABEL)));
        assert!(labels.contains(&format!("{LABEL_PID}={}", std::process::id())));
        assert!(labels.contains(&format!("{LABEL_STARTED}=9")));
        assert!(labels.contains(&format!("{LABEL_HOST}=000000000000001f")));
        let uid = rustix::process::geteuid().as_raw();
        assert!(labels.contains(&format!("{LABEL_UID}={uid}")));
        assert!(!labels.iter().any(|l| l.starts_with(LABEL_MACHINE)));
        let labels = creator_labels(Some(creator.with_machine(Some(0xd), Some(0xe))));
        assert!(labels.contains(&format!("{LABEL_MACHINE}=000000000000000d")));
        assert!(labels.contains(&format!("{LABEL_BOOT}=000000000000000e")));
        let unidentified = creator_labels(None);
        assert!(!unidentified.iter().any(|l| l.starts_with(LABEL_HOST)));
    }
}
