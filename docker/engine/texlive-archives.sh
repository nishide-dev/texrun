#!/bin/sh
# TeX Live packages that are not taken from Debian (#71), for the engine
# image (docker/engine/Dockerfile) and the dev image (docker/dev/Dockerfile).
#
# Debian ships them only in texlive-latex-extra / texlive-fonts-extra, whose
# source package (texlive-extra) has a 2.8 GB .orig.tar.xz: over the 2 GiB
# limit of the release asset with the image's sources
# (.github/workflows/engine-image.yml), and texlive-fonts-extra alone is
# 1.7 GB installed. Instead, the few packages needed are taken from the
# frozen TeX Live 2024 repository (tlnet-final, the TeX Live of Debian
# trixie), which never changes, and each archive is checked against the
# SHA-256 in texlive-archives.sha256.
#
#   sh texlive-archives.sh fetch <list> <dir>
#     Downloads every archive of <list> (texlive-archives.sha256) into <dir>
#     and checks it. Needs curl and sha256sum. Also used for the sources
#     release asset.
#   sh texlive-archives.sh install <dir> <texmf>
#     Unpacks the archives of <dir> into the TeX tree <texmf> (TEXMFLOCAL):
#     the run files (<pkg>.tar.xz) entirely, the license and README files of
#     the documentation (<pkg>.doc.tar.xz), nothing of the sources
#     (<pkg>.source.tar.xz, only in the sources release asset). Writes
#     <texmf>/web2c/updmap.cfg with the font maps the packages declare
#     (tlpobj `execute addMap`). Needs tar and xz. Afterwards, run
#     `mktexlsr <texmf> && updmap-sys` where TeX Live is installed.
set -eu

# The frozen TeX Live 2024 repository; the mirrors are tried in this order.
MIRRORS="https://ftp.math.utah.edu/pub/tex/historic/systems/texlive/2024/tlnet-final/archive
https://ftp.tu-chemnitz.de/pub/tug/historic/systems/texlive/2024/tlnet-final/archive
https://mirrors.tuna.tsinghua.edu.cn/tex-historic-archive/systems/texlive/2024/tlnet-final/archive"

fetch() {
    list="$1"
    dir="$2"
    mkdir -p "${dir}"
    while read -r sum file; do
        [ -n "${file}" ] || continue
        ok=0
        for mirror in ${MIRRORS}; do
            if curl -fsSL --retry 3 --max-time 300 -o "${dir}/${file}" "${mirror}/${file}" \
                && echo "${sum}  ${dir}/${file}" | sha256sum --check --quiet --strict -; then
                ok=1
                break
            fi
            echo "warning: ${file} from ${mirror} failed" >&2
            rm -f "${dir}/${file}"
        done
        if [ "${ok}" != 1 ]; then
            echo "error: cannot fetch ${file} with SHA-256 ${sum}" >&2
            exit 1
        fi
        echo "${file}: ok"
    done < "${list}"
}

install() {
    dir="$1"
    texmf="$2"
    mkdir -p "${texmf}/web2c"
    for archive in "${dir}"/*.tar.xz; do
        case "${archive}" in
            *.source.tar.xz) ;;
            *.doc.tar.xz)
                tar -tJf "${archive}" | grep -E '/(OFL\.txt|LICENSE[^/]*|COPYING[^/]*|README[^/]*)$' \
                    | xargs -r tar -xJf "${archive}" --no-same-owner -C "${texmf}"
                ;;
            *) tar -xJf "${archive}" --no-same-owner -C "${texmf}" ;;
        esac
    done
    # Font maps (`execute addMap zi4.map`): updmap-sys merges every
    # updmap.cfg, so this one only adds them.
    cat "${texmf}"/tlpkg/tlpobj/*.tlpobj \
        | sed -n 's/^execute add\(Mixed\)\{0,1\}Map \(.*\)$/\1Map \2/p' \
        > "${texmf}/web2c/updmap.cfg"
    # Directories and files readable by everyone (TeX runs as another user).
    chmod -R a+rX,go-w "${texmf}"
}

case "${1:-}" in
    fetch) fetch "$2" "$3" ;;
    install) install "$2" "$3" ;;
    *)
        echo "usage: $0 fetch <list> <dir> | install <dir> <texmf>" >&2
        exit 2
        ;;
esac
