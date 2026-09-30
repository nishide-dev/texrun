# texrun development / integration test image.
#
# - Rust toolchain: must match `rust-toolchain.toml` (channel = "1.98.1").
# - TeX Live: minimal Debian packages needed by the integration fixtures (#10).
# - Preview tools: MuPDF (`mutool`) and Poppler (`pdftoppm` / `pdfinfo`) for #8.
#
# Usage: see docs/development.md (`docker compose run --rm dev <command>`).

ARG RUST_VERSION=1.98.1
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE}

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
