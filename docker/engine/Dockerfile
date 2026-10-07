# texrun engine image for the container backend (`--backend container`,
# docs/security.md §4).
#
# A minimal runtime image, separate from the dev image (docker/dev): no Rust
# toolchain and no compilers. texrun starts it with a read-only root
# filesystem, no network, no capabilities and a non-root user (see
# crates/texrun-sandbox).
#
#   docker build -t texrun-engine:latest docker/engine
#
# What texrun expects from the image:
# - /usr/bin/latexmk, and pdflatex / bibtex / makeindex in /usr/bin;
# - /usr/bin/timeout (coreutils), which bounds the lifetime of the container
#   even if texrun itself is killed;
# - /usr/bin/prlimit (util-linux), which sets RLIMIT_AS (the runtimes'
#   --ulimit does not take it);
# - /bin/sh (a POSIX shell), which reports whether the compile reached the
#   container's process limit (#49);
# - nothing of its own at /workspace or /texrun (texrun mounts there).
#
# - /usr/bin/mutool (MuPDF) and /usr/bin/pdfinfo + /usr/bin/pdftoppm
#   (Poppler) for the page previews, and /usr/bin/sleep (coreutils), which
#   keeps the preview container alive while texrun runs the tools in it
#   (#46).
#
# The TeX Live and preview packages (and the TeX Live packages of the first
# stage, from texlive-archives.sha256) are the same as in the dev image, so
# the #10 fixtures and the preview tests behave the same with both
# backends. MuPDF is AGPL-licensed and Poppler GPL-licensed: they are
# installed from Debian as separate programs, and texrun only starts them
# (see docs/engine-image.md for what distributing this image means).

# TeX Live packages that Debian only has in texlive-latex-extra /
# texlive-fonts-extra / texlive-science (texlive-archives.sha256):
# - for the ACL template (acl-org/acl-style-files, #71): inconsolata
#   (\usepackage{inconsolata}: Type1 fonts and their map) and upquote,
#   which it loads; mwe (the example-image-* files that templates include);
# - common in papers (#69): siunitx, multirow, makecell, threeparttable,
#   enumitem, cleveref, algorithms (algorithm / algorithmic), algorithmicx
#   (algpseudocode), algorithm2e (with ifoddpage and relsize, which it
#   loads), wrapfig, placeins, units (nicefrac), xurl, soul, comment.
# Those Debian packages come from the texlive-extra source package, whose
# 2.8 GB .orig.tar.xz alone is over the 2 GiB limit of the sources release
# asset (and texlive-fonts-extra is 1.7 GB installed). The packages are
# taken from the frozen TeX Live 2024 repository instead (the TeX Live of
# trixie) and checked against texlive-archives.sha256; see
# texlive-archives.sh. Only the unpacked files are copied into the image:
# curl and xz stay in this stage.
#
# Both stages are pinned by digest (multi-arch index) to the same image;
# Dependabot (docker ecosystem) updates them. The sources job of
# .github/workflows/engine-image.yml takes the first FROM as the base of
# the image.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS texlive-archives
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils
COPY texlive-archives.sha256 texlive-archives.sh /tmp/
RUN sh /tmp/texlive-archives.sh fetch /tmp/texlive-archives.sha256 /tmp/archives \
    && sh /tmp/texlive-archives.sh install /tmp/texlive-archives.sha256 /tmp/archives /texmf

FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

# Keep documentation, man pages and non-English locales out of the image.
RUN printf '%s\n' \
      'path-exclude=/usr/share/doc/*' \
      'path-include=/usr/share/doc/*/copyright' \
      'path-exclude=/usr/share/man/*' \
      'path-exclude=/usr/share/info/*' \
      'path-exclude=/usr/share/locale/*' \
      'path-include=/usr/share/locale/locale.alias' \
      'path-exclude=/usr/share/texlive/texmf-dist/doc/*' \
      'path-exclude=/usr/share/texmf/doc/*' \
      > /etc/dpkg/dpkg.cfg.d/01-texrun-nodoc

# Fonts (#65): texrun sets MKTEXPK=0 (docs/security.md §3.4), so a font
# without a Type1 file fails the compile ("Font ... not found") instead of
# being generated as a bitmap. texlive-latex-base only brings Computer
# Modern (OT1) in Type1, but current LaTeX takes \textbullet (itemize) and
# the other textcomp symbols from TS1, and fontenc T1 is common:
# - cm-super: the Type1 EC / TC fonts (T1 and TS1) at every size
#   (cm-super-minimal only has the 10pt ones, so 11pt documents or headings
#   still fail);
# - lmodern: the LaTeX support of Latin Modern (\usepackage{lmodern}); the
#   fonts themselves (fonts-lmodern) come with texlive-latex-base;
# - texlive-fonts-recommended: the PSNFSS fonts (Times, Helvetica, Courier,
#   Palatino, ... for mathptmx, helvet, mathpazo); it comes from the same
#   source package as texlive-base, so it adds nothing to the sources.
# Graphics (#69): texlive-pictures is pgf / TikZ (which beamer, from
# texlive-latex-recommended, needs), pgfplots, tikz-cd and the like. It
# also comes from texlive-base; its dependency python3 (for a few
# scripts that TeX never runs: texrun disables shell escape) adds the
# python3.13 sources.
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
         latexmk \
         texlive-latex-base \
         texlive-latex-recommended \
         texlive-fonts-recommended \
         texlive-pictures \
         cm-super \
         lmodern \
         mupdf-tools \
         poppler-utils \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* /var/log/apt/* /var/log/dpkg.log

# The TeX Live packages of the first stage, in TEXMFLOCAL. kpathsea finds
# them through the ls-R database (TeX cannot regenerate it at run time:
# read-only filesystem, MKTEX*=0), and pdfTeX finds their fonts in the font
# maps, which updmap-sys regenerates with the maps of the packages added.
COPY --from=texlive-archives /texmf /usr/local/share/texmf
RUN mktexlsr /usr/local/share/texmf \
    && updmap-sys --quiet \
    && grep -q '^t1-zi4r-0 ' /var/lib/texmf/fonts/map/pdftex/updmap/pdftex.map \
    && rm -f /var/lib/texmf/web2c/updmap.log

# The user when texrun does not pass one, and the one texrun passes when it
# runs as root itself (otherwise it passes the host user's uid and gid):
# never root.
RUN useradd --uid 10001 --user-group --no-create-home --home-dir /nonexistent \
      --shell /usr/sbin/nologin texrun

# Only PATH: texrun passes the rest of the engine's environment itself.
ENV PATH=/usr/bin:/bin
USER 10001:10001
WORKDIR /
