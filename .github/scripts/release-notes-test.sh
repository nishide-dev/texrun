#!/usr/bin/env bash
# Checks the "What's changed" part of release-notes.sh against
# release-notes-test.expected.md, on a throwaway repository whose commits
# look like the squash merges on main: the PR title as the commit title and
# the PR body (or Dependabot's) as the commit body.
#
#   .github/scripts/release-notes-test.sh            # compare
#   .github/scripts/release-notes-test.sh --update   # rewrite the expected file
#
# Needs git, git-cliff (or $GIT_CLIFF) and jq. Run by release-notes.yml.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "${here}/../.." && pwd)"
expected="${here}/release-notes-test.expected.md"

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
repo="${tmp}/repo"
mkdir -p "${repo}/.github/scripts"
cp "${root}/cliff.toml" "${repo}/"
cp "${here}/release-notes.sh" "${repo}/.github/scripts/"

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE=2026-01-01T00:00:00Z GIT_COMMITTER_DATE=2026-01-01T00:00:00Z
g() { git -C "${repo}" -c init.defaultBranch=main -c commit.gpgsign=false -c tag.gpgsign=false "$@"; }
commit() { g commit -q --allow-empty -F -; }

g init -q
g add -A
commit <<'EOF'
feat(cli): add compile command (#1)

## 概要

compile コマンドを追加する（#0 の続き）。

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
g tag v0.1.0

# Dependabot: a single update and a group, with their bodies.
commit <<'EOF'
chore(deps): bump serde from 1.0.1 to 1.0.2 (#2)

Bumps [serde](https://github.com/serde-rs/serde) from 1.0.1 to 1.0.2.
- [Release notes](https://github.com/serde-rs/serde/releases)
- [Commits](https://github.com/serde-rs/serde/compare/v1.0.1...v1.0.2)

---
updated-dependencies:
- dependency-name: serde
  dependency-version: 1.0.2
  dependency-type: direct:production
  update-type: version-update:semver-patch
...

Signed-off-by: dependabot[bot] <support@github.com>
EOF
commit <<'EOF'
chore(deps): bump the github-actions group with 2 updates (#3)

Bumps the github-actions group with 2 updates: [actions/checkout](https://github.com/actions/checkout) and [actions/cache](https://github.com/actions/cache).

You can trigger a rebase of this PR by commenting `@dependabot rebase` (#9).

Signed-off-by: dependabot[bot] <support@github.com>
EOF
# A dependency update without a PR.
commit <<'EOF'
chore(deps): update Cargo.lock
EOF
# A breaking change footer in Japanese, followed by the attribution.
commit <<'EOF'
feat(cli)!: rename --json flag to --format json (#4)

## 概要

`--json` を `--format json` にする。

BREAKING CHANGE: `--json` は削除され、`--format json` に置き換えられた。

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
# Not a Conventional Commit (GitHub's revert button).
commit <<'EOF'
Revert "fix(core): handle empty logs (#5)" (#6)

This reverts commit 0123456789abcdef0123456789abcdef01234567.
EOF
commit <<'EOF'
fix(diagnostics): report the error @octocat found (#7)

Reported by @octocat.
EOF
g tag v0.2.0

actual="${tmp}/actual.md"
(cd "${repo}" && bash .github/scripts/release-notes.sh v0.2.0 "sha256:$(printf '0%.0s' {1..64})") 2>/dev/null \
  | sed '/^## Install/,$d' > "${actual}"

if [ "${1:-}" = --update ]; then
  cp "${actual}" "${expected}"
  echo "updated ${expected}"
elif diff -u "${expected}" "${actual}"; then
  echo "release notes: as expected"
else
  echo "release notes differ from ${expected} (run with --update to accept)" >&2
  exit 1
fi
