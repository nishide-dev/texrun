# Installing texrun

Installing software changes the user's machine: tell the user what is
missing and show these commands; run them only if the user asks you to.

texrun runs on Linux and macOS (not Windows) and is built with Rust 1.98 or
newer:

```bash
cargo install --locked --git https://github.com/nishide-dev/texrun --tag v0.1.0 texrun
texrun --version
```

Use the newest release tag from https://github.com/nishide-dev/texrun/releases
in place of `v0.1.0`.

Then one of the backends:

- **Container backend (recommended, required for untrusted documents):**
  Docker 20.10+ running as root (Docker Desktop and OrbStack work) or
  rootless Podman 4+, and the engine image of the same version as
  `texrun --version`. texrun never pulls images; pull it once:

  ```bash
  docker pull ghcr.io/nishide-dev/texrun-engine:0.1.0
  ```

  The image contains TeX Live, latexmk and the preview tools. Rootless
  Docker is not supported.

- **Host backend:** TeX Live with `latexmk` on PATH. For page previews,
  also MuPDF (`mutool`) or Poppler (`pdfinfo` and `pdftoppm`). Missing LaTeX
  packages are installed with TeX Live (e.g. `tlmgr install <package>`),
  not by texrun.

## Checking the setup

```bash
texrun --version
docker image ls ghcr.io/nishide-dev/texrun-engine
```

A compile that fails with `error.kind` `unavailable` names what is missing
in `error.message` and `error.hint`.
