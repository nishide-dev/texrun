#!/bin/sh
# Fetches the ACL template (https://github.com/acl-org/acl-style-files) at a
# pinned commit for the `acl_template` test of texrun-texlive (#71):
#
#   sh .github/scripts/fetch-acl-style-files.sh <dir>
#   TEXRUN_ACL_STYLE_FILES=<dir> cargo test -p texrun-texlive --test scenarios acl_template
#
# The repository has no license file and acl.sty / acl_latex.tex carry no
# license, so the template is not bundled as a fixture; the test compiles
# this checkout instead. <dir> must not exist yet; it gets the files of the
# commit, without .git. Needs git and network access (CI fetches it in a
# step of its own, before the tests).
#
# To move to a newer template, change COMMIT (a commit of the master
# branch) and check that the test still passes on both backends.
set -eu

REPOSITORY=https://github.com/acl-org/acl-style-files.git
# master, 2026-06-29 ("mention template for XeLaTeX in README.md").
COMMIT=d5adc823ff0f80f98c80405ca0ab66c68e684409

dir="$1"
if [ -e "${dir}" ]; then
    echo "error: ${dir} already exists" >&2
    exit 1
fi
mkdir -p "${dir}"
git -C "${dir}" init -q
# Retried: a passing GitHub outage should not fail CI.
attempt=1
until git -C "${dir}" fetch -q --depth 1 "${REPOSITORY}" "${COMMIT}"; do
    if [ "${attempt}" -ge 3 ]; then
        echo "error: cannot fetch ${COMMIT} of ${REPOSITORY}" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 10
done
git -C "${dir}" -c advice.detachedHead=false checkout -q FETCH_HEAD
head="$(git -C "${dir}" rev-parse HEAD)"
if [ "${head}" != "${COMMIT}" ]; then
    echo "error: fetched ${head}, not ${COMMIT}" >&2
    exit 1
fi
rm -rf "${dir}/.git"
echo "acl-org/acl-style-files ${COMMIT} in ${dir}"
