//! The environment of a child process (docs/security.md §3.4).

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// The complete environment of a child: the supervisor applies
/// `env_clear()` and then sets exactly these variables.
///
/// Names are unique; setting a name again replaces its value. Order is
/// preserved (insertion order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvAllowlist {
    vars: Vec<(OsString, OsString)>,
}

impl EnvAllowlist {
    /// An empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name` to `value`, replacing an earlier value.
    #[must_use]
    pub fn with(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.set(name, value);
        self
    }

    /// Sets `name` to `value`, replacing an earlier value.
    pub fn set(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        let (name, value) = (name.into(), value.into());
        match self.vars.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => self.vars.push((name, value)),
        }
    }

    /// Sets `PATH` to `raw` without its empty and relative entries
    /// ([`sanitize_path`]).
    #[must_use]
    pub fn with_path(self, raw: &OsStr) -> Self {
        self.with("PATH", sanitize_path(raw))
    }

    /// The value of `name`, if set.
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_os_str())
    }

    /// The variables, in insertion order.
    pub fn vars(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(n, v)| (n.as_os_str(), v.as_os_str()))
    }
}

/// `path` (a `PATH`-style list) with empty and relative entries removed.
///
/// Children run with a working directory that may contain untrusted files
/// (the workspace, a scratch directory). An empty or relative entry (`.`,
/// `bin`) would let such a file be run in place of a program looked up in
/// `PATH`.
pub fn sanitize_path(path: &OsStr) -> OsString {
    let kept: Vec<PathBuf> = env::split_paths(path).filter(|p| p.is_absolute()).collect();
    // Entries come from `split_paths`, so they contain no separator.
    env::join_paths(kept).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_drops_relative_and_empty_entries() {
        let sanitized = sanitize_path(OsStr::new("/usr/bin::.:bin:/opt/tex/bin:./x"));
        assert_eq!(sanitized, "/usr/bin:/opt/tex/bin");
        assert_eq!(sanitize_path(OsStr::new("")), "");
        assert_eq!(sanitize_path(OsStr::new(".:")), "");
    }

    #[test]
    fn names_are_unique_and_ordered() {
        let env = EnvAllowlist::new()
            .with("B", "1")
            .with("A", "2")
            .with("B", "3")
            .with_path(OsStr::new(".:/bin"));
        let vars: Vec<_> = env.vars().collect();
        assert_eq!(
            vars,
            [
                (OsStr::new("B"), OsStr::new("3")),
                (OsStr::new("A"), OsStr::new("2")),
                (OsStr::new("PATH"), OsStr::new("/bin")),
            ]
        );
        assert_eq!(env.get("A"), Some(OsStr::new("2")));
        assert_eq!(env.get("HOME"), None);
    }
}
