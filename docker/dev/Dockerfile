# texrun development / integration test image.
#
# - Rust toolchain: must match `rust-toolchain.toml` (channel = "1.98.1").
# - TeX Live: minimal Debian packages needed by the integration fixtures (#10).
# - Preview tools: MuPDF (`mutool`) and Poppler (`pdftoppm` / `pdfinfo`) for #8.
#
# Usage: see docs/development.md (`docker compose run --rm dev <command>`).
# CI builds the same image for the TeX Live integration job (ci.yml).

# The build context is `docker/` (compose.yaml, ci.yml), so that this image
# installs the same TeX Live packages from outside Debian as the engine
# image: docker/engine/texlive-archives.sh and texlive-archives.sha256
# (#69, #71; see docker/engine/Dockerfile).
#
# The base image is pinned by digest (multi-arch index) so that CI and local
# builds start from the same image. Dependabot (docker ecosystem) updates the
# digest; bump the tag by hand together with `rust-toolchain.toml`. Both
# stages use the same image.
FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS texlive-archives
RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils
COPY engine/texlive-archives.sha256 engine/texlive-archives.sh /tmp/
RUN sh /tmp/texlive-archives.sh fetch /tmp/texlive-archives.sha256 /tmp/archives \
    && sh /tmp/texlive-archives.sh install /tmp/archives /texmf

FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7

# Keep documentation, man pages and non-English locales out of the image.
# TeX Live doc packages are large; this also strips docs shipped inside
# regular packages.
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

# TeX Live (latexmk + pdflatex + bibtex) and PDF preview tools.
# `texlive-binaries` (pulled in by texlive-latex-base) provides `bibtex`.
# The fonts (Type1 fonts for T1 / TS1, Latin Modern and PSNFSS; #65) and
# texlive-pictures (pgf / TikZ; #69) are the same as in the engine image;
# see docker/engine/Dockerfile.
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

# The TeX Live packages of the first stage, in TEXMFLOCAL, as in the engine
# image.
COPY --from=texlive-archives /texmf /usr/local/share/texmf
RUN mktexlsr /usr/local/share/texmf \
    && updmap-sys --quiet \
    && grep -q '^t1-zi4r-0 ' /var/lib/texmf/fonts/map/pdftex/updmap/pdftex.map \
    && rm -f /var/lib/texmf/web2c/updmap.log

# Pre-install the components listed in `rust-toolchain.toml` so rustup does
# not download the toolchain again when it is picked up inside the container.
RUN rustup component add rustfmt clippy \
    && rustup toolchain list \
    && cargo --version \
    && rustfmt --version \
    && cargo clippy --version \
    && rm -rf /usr/local/rustup/downloads/* /usr/local/rustup/tmp/*

# CARGO_TARGET_DIR is set in compose.yaml because it depends on the mount
# layout (the repo is bind-mounted at /workspace).
WORKDIR /workspace

CMD ["bash"]
