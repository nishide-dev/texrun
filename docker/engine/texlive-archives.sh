#!/bin/sh
# TeX Live packages that are not taken from Debian (#69, #71), for the
# engine image (docker/engine/Dockerfile) and the dev image
# (docker/dev/Dockerfile).
#
# Debian ships them only in texlive-latex-extra / texlive-fonts-extra /
# texlive-science, whose source package (texlive-extra) has a 2.8 GB
# .orig.tar.xz: over the 2 GiB limit of the release asset with the image's
# sources (.github/workflows/engine-image.yml). Instead, the packages are
# taken from the frozen TeX Live repository of the same year as Debian's
# TeX Live (tlnet-final, which never changes; TL_YEAR below), and each
# archive is checked against the SHA-256 in texlive-archives.sha256 (the
# list: "<sha256>  <package>[.doc|.source].tar.xz" per line).
#
#   sh texlive-archives.sh fetch <list> <dir>
#     Downloads every archive of <list> into <dir> and checks it (image
#     build, sources release asset). Needs curl and sha256sum.
#   sh texlive-archives.sh install <list> <dir> <texmf>
#     Unpacks the archives of <list> (fetched into <dir>) into the TeX tree
#     <texmf> (TEXMFLOCAL): the run files (<pkg>.tar.xz) entirely, the
#     license and README files of the documentation (<pkg>.doc.tar.xz),
#     nothing of the sources (<pkg>.source.tar.xz, only in the sources
#     release asset). Writes <texmf>/web2c/updmap.cfg with the font maps
#     the packages declare (tlpobj `execute addMap`), and sets every mtime
#     to 0 so that the image layer is the same on every build. Needs tar
#     and xz. Afterwards, run `mktexlsr <texmf> && updmap-sys` where TeX
#     Live is installed.
#
# Making and checking the list (when it changes; not at build time), with
# the TeX Live database signed by the TeX Live key (docs/development.md):
#
#   sh texlive-archives.sh make-list <dir> <package>...  > texlive-archives.sha256
#     Prints the list for <package>...: every container (run, doc, source)
#     that the database has for each, after checking its SHA-512 against
#     the database.
#   sh texlive-archives.sh verify-tlpdb <list> <dir>
#     Checks that <list> has exactly the containers of its packages and
#     that each archive's SHA-512 is the one of the database; prints the
#     packages' dependencies and licenses (to check by hand).
#   Both need curl, gpg, sha256sum / sha512sum and awk; <dir> gets the
#   database and the archives.
set -eu

# The TeX Live of the Debian release of the images (trixie: TeX Live 2024,
# 2024.20250309, the snapshot of tlnet-final). Change it together with the
# Debian release, and make the list again (make-list).
TL_YEAR=2024
# The mirrors are tried in this order.
MIRRORS="https://ftp.math.utah.edu/pub/tex/historic/systems/texlive/${TL_YEAR}/tlnet-final
https://ftp.tu-chemnitz.de/pub/tug/historic/systems/texlive/${TL_YEAR}/tlnet-final
https://mirrors.tuna.tsinghua.edu.cn/tex-historic-archive/systems/texlive/${TL_YEAR}/tlnet-final"
# The TeX Live key (https://www.tug.org/texlive/gpg.html): its primary key
# must have made the database's signature.
TL_KEY_URL=https://www.tug.org/texlive/files/texlive.asc
TL_KEY_FINGERPRINT=C78B82D8C79512F79CC0D7C80D5E5D9106BAB6BC

die() {
    echo "error: $*" >&2
    exit 1
}

# curl over HTTPS only, also after redirects.
download() {
    curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 \
        --retry 3 --max-time 300 -o "$2" "$1"
}

# Prints the checked lines of <list> as "<sum> <file>": every line must be
# "<64 hex digits>  <name>[.doc|.source].tar.xz" (empty lines are
# allowed), the last one even without a final newline; fails on anything
# else, on duplicates and on an empty list.
read_list() {
    n=0
    seen=" "
    while read -r sum file rest || [ -n "${sum:-}" ]; do
        if [ -z "${sum}" ]; then
            continue
        fi
        bad=
        [ -z "${rest}" ] || bad=1
        case "${sum}" in
            *[!0-9a-f]*) bad=1 ;;
            *) [ "${#sum}" -eq 64 ] || bad=1 ;;
        esac
        case "${file}" in
            '' | */* | .* | *[!A-Za-z0-9._-]*) bad=1 ;;
            *.tar.xz) ;;
            *) bad=1 ;;
        esac
        [ -z "${bad}" ] || die "${1}: malformed line: ${sum} ${file} ${rest}"
        case "${seen}" in
            *" ${file} "*) die "${1}: ${file} is listed twice" ;;
        esac
        seen="${seen}${file} "
        echo "${sum} ${file}"
        n=$((n + 1))
        sum=
    done < "$1"
    [ "${n}" -gt 0 ] || die "${1} lists nothing"
}

fetch() {
    list="$1"
    dir="$2"
    lines="$(read_list "${list}")"
    mkdir -p "${dir}"
    n=0
    while read -r sum file; do
        ok=0
        for mirror in ${MIRRORS}; do
            if download "${mirror}/archive/${file}" "${dir}/${file}" \
                && echo "${sum}  ${dir}/${file}" | sha256sum --check --quiet --strict -; then
                ok=1
                break
            fi
            echo "warning: ${file} from ${mirror} failed" >&2
            rm -f "${dir}/${file}"
        done
        [ "${ok}" = 1 ] || die "cannot fetch ${file} with SHA-256 ${sum}"
        echo "${file}: ok"
        n=$((n + 1))
    done <<EOF
${lines}
EOF
    echo "${n} archives"
}

install() {
    list="$1"
    dir="$2"
    texmf="$3"
    lines="$(read_list "${list}")"
    mkdir -p "${texmf}/web2c"
    runs=0
    while read -r sum file; do
        archive="${dir}/${file}"
        # The archive fetch checked, not another one.
        echo "${sum}  ${archive}" | sha256sum --check --quiet --strict - \
            || die "${archive} is not the archive of ${list}"
        case "${file}" in
            *.source.tar.xz) ;;
            *.doc.tar.xz)
                tar -tJf "${archive}" | grep -E '/(OFL\.txt|LICENSE[^/]*|COPYING[^/]*|README[^/]*)$' \
                    | xargs -r tar -xJf "${archive}" --no-same-owner -C "${texmf}"
                ;;
            *)
                tar -xJf "${archive}" --no-same-owner -C "${texmf}"
                runs=$((runs + 1))
                ;;
        esac
    done <<EOF
${lines}
EOF
    # Every run archive brings its tlpobj.
    tlpobjs="$(find "${texmf}/tlpkg/tlpobj" -name '*.tlpobj' | wc -l)"
    [ "${tlpobjs}" -eq "${runs}" ] || die "${runs} run archives, but ${tlpobjs} tlpobj files"
    # Font maps (`execute addMap zi4.map`): updmap-sys merges every
    # updmap.cfg, so this one only adds them.
    cat "${texmf}"/tlpkg/tlpobj/*.tlpobj \
        | sed -n 's/^execute add\(Mixed\)\{0,1\}Map \(.*\)$/\1Map \2/p' \
        > "${texmf}/web2c/updmap.cfg"
    # Readable by everyone (TeX runs as another user), and the same files
    # with the same metadata on every build.
    chmod -R a+rX,go-w "${texmf}"
    find "${texmf}" -exec touch -h -d @0 {} +
    echo "${runs} packages in ${texmf}"
}

# Downloads the TeX Live database into <dir> and checks it: the signature
# of its SHA-512 by the TeX Live key, then its SHA-512.
tlpdb() {
    dir="$1"
    mkdir -p "${dir}"
    ok=0
    for mirror in ${MIRRORS}; do
        if download "${mirror}/tlpkg/texlive.tlpdb" "${dir}/texlive.tlpdb" \
            && download "${mirror}/tlpkg/texlive.tlpdb.sha512" "${dir}/texlive.tlpdb.sha512" \
            && download "${mirror}/tlpkg/texlive.tlpdb.sha512.asc" "${dir}/texlive.tlpdb.sha512.asc"; then
            ok=1
            break
        fi
        echo "warning: the database from ${mirror} failed" >&2
    done
    [ "${ok}" = 1 ] || die "cannot fetch the TeX Live database"
    download "${TL_KEY_URL}" "${dir}/texlive.asc"
    gnupg="$(mktemp -d)"
    GNUPGHOME="${gnupg}" gpg --batch --quiet --import "${dir}/texlive.asc" 2>/dev/null
    status="$(GNUPGHOME="${gnupg}" gpg --batch --status-fd 1 \
        --verify "${dir}/texlive.tlpdb.sha512.asc" "${dir}/texlive.tlpdb.sha512" 2>/dev/null || true)"
    rm -rf "${gnupg}"
    # VALIDSIG <signing key> ... <primary key>
    echo "${status}" | grep -q "^\[GNUPG:\] VALIDSIG .* ${TL_KEY_FINGERPRINT}\$" \
        || die "the database is not signed by the TeX Live key ${TL_KEY_FINGERPRINT}"
    (cd "${dir}" && sha512sum --check --quiet --strict texlive.tlpdb.sha512) \
        || die "texlive.tlpdb does not match its signed SHA-512"
    echo "texlive.tlpdb: signed by ${TL_KEY_FINGERPRINT}" >&2
}

# Prints "<file> <sha512>" for every container of package $2 in database
# $1, and "depend <name>" / "license <...>" lines.
containers() {
    awk -v pkg="$2" '
        /^name / { inpkg = ($2 == pkg); found = found || inpkg; next }
        /^$/ { inpkg = 0; next }
        !inpkg { next }
        $1 == "containerchecksum" { print pkg ".tar.xz", $2 }
        $1 == "doccontainerchecksum" { print pkg ".doc.tar.xz", $2 }
        $1 == "srccontainerchecksum" { print pkg ".source.tar.xz", $2 }
        $1 == "depend" { print "depend", $2 }
        $1 == "catalogue-license" { $1 = ""; print "license" $0 }
        END { if (!found) exit 1 }
    ' "$1" || die "no package $2 in the TeX Live database"
}

# Checks <dir>/<file> against the SHA-512 of the database.
check_sha512() {
    echo "$2  $1" | sha512sum --check --quiet --strict - \
        || die "$(basename "$1"): its SHA-512 is not the one of the signed database"
}

make_list() {
    dir="$1"
    shift
    tlpdb "${dir}"
    for pkg in "$@"; do
        info="$(containers "${dir}/texlive.tlpdb" "${pkg}")"
        echo "${info}" | while read -r file sum; do
            case "${file}" in depend | license) continue ;; esac
            ok=0
            for mirror in ${MIRRORS}; do
                if download "${mirror}/archive/${file}" "${dir}/${file}"; then
                    ok=1
                    break
                fi
            done
            [ "${ok}" = 1 ] || die "cannot fetch ${file}"
            check_sha512 "${dir}/${file}" "${sum}"
            echo "$(sha256sum "${dir}/${file}" | cut -d' ' -f1)  ${file}"
        done
    done
}

verify_tlpdb() {
    list="$1"
    dir="$2"
    lines="$(read_list "${list}")"
    tlpdb "${dir}"
    fetch "${list}" "${dir}" >/dev/null
    packages="$(echo "${lines}" | sed 's/^[^ ]* //; s/\.tar\.xz$//; s/\.doc$//; s/\.source$//' | sort -u)"
    expected=
    for pkg in ${packages}; do
        info="$(containers "${dir}/texlive.tlpdb" "${pkg}")"
        echo "${pkg}:$(echo "${info}" | sed -n 's/^license//p') depends on: $(echo "${info}" | sed -n 's/^depend //p' | tr '\n' ' ')"
        while read -r file sum; do
            case "${file}" in depend | license) continue ;; esac
            check_sha512 "${dir}/${file}" "${sum}"
            expected="${expected}${file}
"
        done <<EOF
${info}
EOF
    done
    listed="$(echo "${lines}" | cut -d' ' -f2 | sort)"
    expected="$(printf '%s' "${expected}" | sort)"
    if [ "${listed}" != "${expected}" ]; then
        echo "listed:" >&2
        echo "${listed}" >&2
        echo "containers in the database:" >&2
        echo "${expected}" >&2
        die "${list} does not have exactly the containers of its packages"
    fi
    echo "${list}: $(echo "${listed}" | wc -l) archives of $(echo "${packages}" | wc -w) packages match the signed TeX Live ${TL_YEAR} database"
}

case "${1:-}" in
    fetch) [ $# -eq 3 ] || die "usage: $0 fetch <list> <dir>"; fetch "$2" "$3" ;;
    install) [ $# -eq 4 ] || die "usage: $0 install <list> <dir> <texmf>"; install "$2" "$3" "$4" ;;
    make-list) [ $# -ge 3 ] || die "usage: $0 make-list <dir> <package>..."; shift; make_list "$@" ;;
    verify-tlpdb) [ $# -eq 3 ] || die "usage: $0 verify-tlpdb <list> <dir>"; verify_tlpdb "$2" "$3" ;;
    *)
        echo "usage: $0 fetch <list> <dir> | install <list> <dir> <texmf> | make-list <dir> <package>... | verify-tlpdb <list> <dir>" >&2
        exit 2
        ;;
esac
