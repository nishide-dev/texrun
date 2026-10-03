//! Reclaiming containers that a killed texrun left behind (#49).
//!
//! Every container texrun creates carries, besides [`LABEL`](crate::LABEL),
//! labels naming its creator ([`Creator`]): its PID, the start time of that
//! process (Linux), the host the PID belongs to, and the effective uid of
//! texrun. [`Runtime::reclaim_left_containers`] removes a container only if
//! all of these hold:
//!
//! - it has every label, with the uid and host of this texrun;
//! - its name is the one texrun gives a container of that PID
//!   (`texrun-<pid>-...`);
//! - its creator is gone ([`Creator::is_gone`]): no process has the PID
//!   any more, or one with another start time (the PID was reused);
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
use std::time::Duration;

use crate::error::SandboxError;
use crate::owner::Creator;
use crate::runtime::Runtime;

/// Label with the PID of the texrun process that created the container.
pub const LABEL_PID: &str = "org.texrun.sandbox.pid";
/// Label with the start time of that process ([`Creator::started`]).
pub const LABEL_STARTED: &str = "org.texrun.sandbox.started";
/// Label with the host of that PID ([`Creator::host`], 16 hex digits).
pub const LABEL_HOST: &str = "org.texrun.sandbox.host";
/// Label with the effective uid of that process.
pub const LABEL_UID: &str = "org.texrun.sandbox.uid";

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
    }
    labels
}

/// One container as `inspect` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
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
    if host != me.host() {
        return None;
    }
    let pid: u32 = label(LABEL_PID)?.parse().ok()?;
    let started = match label(LABEL_STARTED) {
        Some(s) => Some(s.parse().ok()?),
        None => None,
    };
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
    stopped.then(|| Creator::new(pid, started, host))
}

/// Parses `inspect --format '{{.Id}} {{.Name}} {{.State.Status}} {{json
/// .Config.Labels}}'`, one container per line.
pub(crate) fn parse_listing(out: &str) -> Vec<Listed> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.trim().splitn(4, ' ');
            let id = fields.next()?.to_owned();
            let name = fields.next()?.to_owned();
            let status = fields.next()?.to_owned();
            let labels: BTreeMap<String, String> =
                serde_json::from_str::<Option<_>>(fields.next()?)
                    .ok()?
                    .unwrap_or_default();
            (!id.is_empty()).then_some(Listed {
                id,
                name,
                status,
                labels,
            })
        })
        .collect()
}

impl Runtime {
    /// Removes the containers that texrun processes of this user on this
    /// host created and left behind when they were killed (see the module
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
        let filters = [
            format!("label={}=1", crate::LABEL),
            format!("label={LABEL_UID}={uid}"),
            format!("label={LABEL_HOST}={:016x}", me.host()),
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
                if !creator.is_gone() {
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
                "--format",
                "{{.Id}} {{.Name}} {{.State.Status}} {{json .Config.Labels}}",
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
            format!("left-id /texrun-{dead}-0-0 exited {labels}\n"),
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
        let out = "abc /texrun-1-0-1 exited {\"org.texrun.sandbox\":\"1\",\"x\":\"a b\"}\n\
                   def name running null\n\
                   broken\n";
        let parsed = parse_listing(out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "abc");
        assert_eq!(parsed[0].name, "/texrun-1-0-1");
        assert_eq!(parsed[0].status, "exited");
        assert_eq!(parsed[0].labels["x"], "a b");
        assert!(parsed[1].labels.is_empty());
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
        let unidentified = creator_labels(None);
        assert!(!unidentified.iter().any(|l| l.starts_with(LABEL_HOST)));
    }
}
