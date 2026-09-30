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
# - nothing of its own at /workspace or /texrun (texrun mounts there).
#
# The TeX Live packages are the same as in the dev image, so the #10
# fixtures behave the same with both backends. MuPDF and Poppler are
# included for running the preview in the sandbox as well (#46).

# Pinned by digest (multi-arch index); Dependabot (docker ecosystem)
# updates it.
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

RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
         latexmk \
         texlive-latex-base \
         texlive-latex-recommended \
         mupdf-tools \
         poppler-utils \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* /var/log/apt/* /var/log/dpkg.log

# The user when texrun does not pass one (it passes the host user's uid
# and gid, or 65534 when texrun runs as root): never root.
RUN useradd --uid 10001 --user-group --no-create-home --home-dir /nonexistent \
      --shell /usr/sbin/nologin texrun

# Only PATH: texrun passes the rest of the engine's environment itself.
ENV PATH=/usr/bin:/bin
USER 10001:10001
WORKDIR /
