#!/bin/sh
# Downloads the Debian source packages of the engine image
# (docker/engine/Dockerfile) at exactly the versions installed in it, for the
# release asset that accompanies every published image
# (.github/workflows/engine-image.yml, README "Engine image"): the complete
# corresponding source of the GPL / AGPL packages in the image (and of all
# the others).
#
# Runs as root in a container of the image's base (debian:trixie-slim at
# the digest of docker/engine/Dockerfile), not in the engine image:
#
#   sh fetch-engine-sources.sh <packages> <out>
#
# <packages>: one "<source package> <source version>" per line, as printed
# in the engine image by
#   dpkg-query -W -f '${source:Package} ${source:Version}\n'
# <out>: gets the source files (.dsc, .orig.tar.*, .debian.tar.* / .diff.gz)
# and SHA256SUMS. Each package is taken from the archive (deb-src) if it
# still has that version, else from snapshot.debian.org; the script fails if
# any package cannot be fetched at exactly its version.
#
# Integrity: from the archive, apt checks the signed Release / Sources
# indexes. From snapshot.debian.org, each file is checked against its SHA-1
# (the address it is served under) and every package against the checksums
# of its .dsc, but the .dsc's own signature is not verified (that would need
# the keys of every past uploader; the keyring drops expired ones), so
# those packages rely on HTTPS to snapshot.debian.org.
set -eu

packages="$1"
out="$2"

export DEBIAN_FRONTEND=noninteractive
sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/debian.sources
apt-get update -qq
apt-get install -y -qq --no-install-recommends dpkg-dev ca-certificates curl jq >/dev/null

mkdir -p "${out}"
cd "${out}"
failed=0
for entry in $(sort -u "${packages}" | tr ' ' '='); do
    name="${entry%%=*}"
    version="${entry#*=}"
    if apt-get source -qq --download-only "${name}=${version}" >/dev/null 2>&1; then
        echo "${name} ${version}: archive"
        continue
    fi
    # The archive no longer has this version (e.g. superseded by a point
    # release): snapshot.debian.org keeps every source package ever uploaded.
    info="https://snapshot.debian.org/mr/package/${name}/${version}/srcfiles?fileinfo=1"
    if files="$(curl -fsSL --retry 3 "${info}" | jq -r '.fileinfo | to_entries[] | "\(.key) \(.value[0].name)"')" \
        && [ -n "${files}" ]; then
        # snapshot.debian.org addresses files by their SHA-1: check it.
        echo "${files}" | while read -r digest file; do
            curl -fsSL --retry 3 -o "${file}" "https://snapshot.debian.org/file/${digest}"
            echo "${digest}  ${file}" | sha1sum --check --quiet --strict -
        done
        echo "${name} ${version}: snapshot.debian.org"
    else
        echo "error: cannot fetch the source of ${name} ${version}" >&2
        failed=1
    fi
done
[ "${failed}" = 0 ]
# Every source package is complete: all the files its .dsc lists are here,
# with the checksums it lists.
for dsc in *.dsc; do
    sed -n '/^Checksums-Sha256:/,/^[^ ]/{/^ /p}' "${dsc}" \
        | awk '{ print $1 "  " $3 }' | sha256sum --check --quiet --strict -
done
echo "$(find . -maxdepth 1 -name '*.dsc' | wc -l) source packages, $(du -sh . | cut -f1)"
sha256sum -- * > SHA256SUMS
