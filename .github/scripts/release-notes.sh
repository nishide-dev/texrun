#!/usr/bin/env bash
# Prints the GitHub release notes of a texrun version (Markdown):
#
#   release-notes.sh <tag> [<digest>]
#
# - What's changed: the commits of the release, by Conventional Commits type
#   (git-cliff, cliff.toml). Those of a stable version are the ones since the
#   previous stable version (prereleases in between are folded in); those of
#   a prerelease, since the previous tag of any kind. The first release has
#   the whole history.
# - Install, engine image (by tag and by <digest>, the digest of the
#   published ghcr.io/nishide-dev/texrun-engine:<version>), verification,
#   and the source tar of the image.
#
# <tag> is v<semver>. If it exists, it must be HEAD or an ancestor of HEAD
# (run it on main or on the tag). If it does not exist yet, the commits since
# the last tag up to HEAD are shown as that version: a preview of the next
# release. Without <digest> (a preview, or an image not published yet), the
# digest is a placeholder.
#
# Needs git (the full history, with tags), git-cliff (or $GIT_CLIFF) and jq.
# Used by .github/workflows/engine-image.yml (release) and
# .github/workflows/release-notes.yml; see docs/development.md.
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <tag> [<digest>]" >&2
  exit 2
fi
tag="$1"
digest="${2:-}"
version="${tag#v}"
if ! [[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: ${tag} is not v<semver>" >&2
  exit 1
fi
if [ -n "${digest}" ] && ! [[ "${digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "error: ${digest} is not a sha256:<hex> digest" >&2
  exit 1
fi

repo="${GITHUB_REPOSITORY:-nishide-dev/texrun}"
server="${GITHUB_SERVER_URL:-https://github.com}"
image="ghcr.io/nishide-dev/texrun-engine"
root="$(git rev-parse --show-toplevel)"

cliff=("${GIT_CLIFF:-git-cliff}" --offline --config "${root}/cliff.toml" --workdir "${root}")
# A stable version lists everything since the previous stable version.
if [[ "${version}" != *-* ]]; then
  cliff+=(--ignore-tags '^v.*-')
fi
if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  if ! git merge-base --is-ancestor "refs/tags/${tag}" HEAD; then
    echo "error: ${tag} is not HEAD or an ancestor of HEAD; run on main or on the tag" >&2
    exit 1
  fi
else
  # Not tagged yet: the unreleased commits, as that version.
  cliff+=(--tag "${tag}")
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
"${cliff[@]}" --context > "${tmp}/all.json"
jq --arg v "${tag}" '[.[] | select(.version == $v)]' "${tmp}/all.json" > "${tmp}/release.json"
if [ "$(jq length "${tmp}/release.json")" -ne 1 ]; then
  echo "error: no release ${tag} in the history (no commits since the previous tag?)" >&2
  exit 1
fi
# (Without the blank lines git-cliff puts before it.)
"${cliff[@]}" --from-context "${tmp}/release.json" | awk 'NF { p = 1 } p' > "${tmp}/changes.md"
# The release page has the version and date already: the heading of the
# release becomes a section of the notes.
if ! head -n 1 "${tmp}/changes.md" | grep -q '^## '; then
  echo "error: unexpected git-cliff output (cliff.toml):" >&2
  cat "${tmp}/changes.md" >&2
  exit 1
fi

if [ -n "${digest}" ]; then
  pinned="${image}@${digest}"
else
  pinned="${image}@sha256:<digest of the published image>"
fi
blob="${server}/${repo}/blob/${tag}"

# cat -s: one blank line between the sections.
{
echo "## What's changed"
tail -n +2 "${tmp}/changes.md"
cat <<EOF

## Install

\`\`\`bash
cargo install --locked --git ${server}/${repo} --tag ${tag} texrun
\`\`\`

See the [README](${blob}/README.md#installation) for the requirements and
the tools each backend needs.

## Engine image

The container backend (\`--backend container\`) runs TeX in the engine image
of this version, \`${image}:${version}\` (linux/amd64, linux/arm64). texrun
never pulls it; pull it once:

\`\`\`bash
docker pull ${image}:${version}
\`\`\`

Published versions are never overwritten. To use exactly this image, pin its
digest:

\`\`\`bash
docker pull ${pinned}
texrun compile --backend container --container-image ${pinned} main.tex
\`\`\`

The image has an SBOM, SLSA provenance and a signed GitHub artifact
attestation:

\`\`\`bash
gh attestation verify oci://${image}:${version} -R ${repo}
\`\`\`

## Source of the engine image

\`texrun-engine-${version}-sources.tar\` (below) has the Debian source packages
(\`.dsc\`, \`.orig.tar.*\`, \`.debian.tar.*\`) of every package in the image,
for both platforms, at exactly the installed versions (\`packages.txt\` and
\`SHA256SUMS\` inside): the corresponding source of its GPL / AGPL programs.
See [docs/engine-image.md](${blob}/docs/engine-image.md#licenses).
EOF
} | cat -s
