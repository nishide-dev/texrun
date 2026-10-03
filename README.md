# texrun

> **Status: early development.** `texrun compile` (TeX Live + latexmk,
> structured diagnostics, PDF and page previews) works on Linux and macOS, but
> the interface may still change before the first release. Progress of the
> first milestone (MVP) is tracked in
> [#13](https://github.com/nishide-dev/texrun/issues/13).

texrun is a frontend layer for compiling and inspecting TeX documents through a
safe, consistent interface, designed to be driven by AI agents and automated
environments as well as by people.

**日本語概要:** texrun は、AI エージェントや自動化環境から安全かつ一貫した
インターフェースで TeX 文書を compile / inspect するための Rust 製 CLI です。
TeX Live + latexmk を backend とし、compile の成否に加えて構造化 diagnostics・
PDF・ページ preview を返します（MVP 実装済み。初回リリース前のため、
インターフェースは変わる可能性があります）。

## Goal

Running `latexmk` directly gives you a PDF and a long, hard-to-parse log.
texrun wraps the TeX toolchain so that callers get:

- a predictable command-line interface with machine-readable (JSON) output,
- structured errors and warnings instead of raw log text,
- the PDF artifact and page previews,
- execution in an isolated temporary workspace with a timeout.

## MVP architecture

```text
Agent / User
    |
    v
texrun CLI
    |
    v
texrun core
    |
    v
engine interface
    |
    v
TeX Live + latexmk
    |
    +--> PDF
    +--> diagnostics
    +--> page previews
```

The engine interface keeps the core independent of a particular TeX
distribution or execution backend, so that other engines or stronger isolation
(e.g. a container worker) can be added later without breaking the core API.

## MVP scope

The first milestone covers:

- TeX Live backend (via `latexmk`)
- local CLI
- isolated temporary workspace for each compile
- structured diagnostics (errors and warnings)
- PDF artifact
- page previews
- compile timeout

## Non-goals (first milestone)

- a full TeX parser implementation
- a web UI
- a hosted SaaS
- installing arbitrary packages during compile
- perfect in-process sandboxing
- support for multiple engines
- Windows support

## CLI

```bash
# Human-readable output
texrun compile main.tex

# One JSON document on stdout (for tools and AI agents)
texrun compile --json main.tex
```

`texrun compile [OPTIONS] <ENTRYPOINT>` copies the project into a temporary
workspace, runs latexmk there with a timeout, renders PNG previews of the
first pages after a successful compile, and copies the PDF, the log and the
previews to the output directory. See `texrun compile --help` for details.

| Option | Default | Meaning |
| --- | --- | --- |
| `--json` | off | Print the result as one JSON document on stdout |
| `-o, --output <DIR>` | `texrun-out/` next to the entrypoint | Where the PDF, log and `preview/page-NNN.png` are copied. Files of the same name are replaced; files from earlier runs are not removed. Symlinks inside the project are never followed on the way there (exit 3, `unsafe_output_path`) |
| `--root <DIR>` | the entrypoint's directory | Project root copied into the workspace; must contain the entrypoint. `/` is refused; `$HOME` and the system temporary directory are refused unless given explicitly |
| `--timeout <DURATION>` | `60s` | Wall-clock limit of the compile (`90`, `90s`, `2m`, `1500ms`) |
| `--keep-workspace` | off | Keep the workspace for debugging; its path is printed on stderr |
| `--source-date-epoch <SECONDS>` | unset | Set `SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE=1` for TeX (fixed PDF dates). The environment variable is not passed through |
| `--no-preview` | off | Do not render page previews |
| `--pages <RANGE>` | first 20 pages | Pages to preview: `N`, `N-M`, `N-`, `-M` (at most 200) |
| `--preview-dpi <DPI>` | `144` | Preview resolution (long edge at most 4096 px) |
| `--preview-backend <BACKEND>` | `auto` | `auto` (MuPDF, else Poppler), `mupdf` or `poppler` |
| `--backend <BACKEND>` | `host` | Where TeX runs: `host` (the host's latexmk, as your user) or `container` (in a hardened container of the engine image; see [Engine backends](#engine-backends)) |
| `--container-runtime <RUNTIME>` | `auto` | With `--backend container`: `auto` (Docker if installed and running, otherwise Podman), `docker` or `podman`. Rootless Docker is not supported (use rootless Podman); see [Engine backends](#engine-backends) |
| `--container-image <IMAGE>` | `ghcr.io/nishide-dev/texrun-engine:<version>` | With `--backend container`: the engine image; must exist locally, texrun never pulls (see [Engine image](#engine-image)) |
| `--cgroup <MODE>` | `auto` | Linux: run latexmk and the preview tools in cgroups of their own (memory, processes, CPU). `auto` uses a delegated cgroup if there is one (e.g. `systemd-run --user --scope -p Delegate=yes texrun ...`), otherwise only the per-process limits apply (see `resource_limits`); `required` fails with exit 3 instead; `off` never uses one |

The workspace never contains VCS metadata, `texrun-out/`, `.texrun/`,
precompiled formats or tool configuration such as `latexmkrc` (texrun never
runs it; a warning is printed when one is left out). An `--output` directory
inside the project is left out too, at any depth (reported with reason
`excluded_path`; any other files kept there are not copied either), unless
it contains the entrypoint (also through a symlink): then it is copied with
the project, including the output of earlier runs, and an info note
`output_contains_entrypoint` says so. TeX cannot read files
above the entrypoint's directory (`\input{../x}`); keep the entrypoint in the
project root.

### Output

- Without `--json`, stdout has the result: diagnostics as
  `file:line: error: message` (errors, then warnings, identical ones merged,
  at most 20 each), a status line with the elapsed time, and the paths of the
  PDF (or the log on failure) and previews. Warnings and errors of texrun
  itself go to stderr. Control, bidi and zero-width characters in file names
  and messages are shown escaped (`\u{202E}`).
- With `--json`, stdout contains exactly one JSON document, also on compile
  failure, timeout and runtime errors (and for usage errors when `--json` is
  on the command line). Strings are not escaped beyond JSON.

### JSON document

```jsonc
{
  "schema_version": 1,
  "texrun_exit_code": 0,             // the exit code of texrun itself (table below)
  // When the compile ran to an outcome (texrun_core::CompileResult):
  "outcome": "succeeded",            // failed | timed_out | cancelled
  "engine": { "name": "texlive", "version": "latexmk 4.86" },
  "exit": { "code": 0 },             // how the latexmk process ended, e.g. { "signal": 9 }
  "elapsed_ms": 812,
  "diagnostics": [
    { "severity": "error", "kind": "undefined_control_sequence",
      "message": "Undefined control sequence \\foo", "file": "chapters/intro.tex", "line": 3 }
  ],
  "artifacts": [
    { "kind": "pdf", "path": "main.pdf", "size_bytes": 11290 },
    { "kind": "log", "path": "main.log", "size_bytes": 2800 },
    { "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 50212 }
  ],
  // Which OS-level limits were in place for latexmk (docs/security.md §3.10):
  "resource_limits": { "rlimits": true, "cgroup": false,
                       "notes": [ "cgroup: no delegated cgroup: ..." ] },
  // After a successful compile, unless --no-preview (texrun_preview::PreviewReport):
  "preview": { "status": "rendered", "backend": "mupdf", "format": "png",
               "pdf": { "page_count": 1, "pages": [ ... ] }, "pages": [ ... ], "notices": [] },
  "output_dir": "/abs/path/texrun-out",     // artifact paths are relative to this
  // Only when copying stopped with an error: produced, but not in output_dir
  "artifacts_not_copied": [ { "kind": "preview", "path": "preview/page-001.png", "page": 1 } ],
  // Advice about the run (not document diagnostics), when there is any
  "notes": [ { "severity": "info", "kind": "parent_directory_input", "message": "..." } ],
  "project": { "root": "/abs/path", "entrypoint": "main.tex" },  // diagnostic files are relative to root
  "workspace": { "excluded": [ { "path": "latexmkrc", "reason": "tool_config" } ],
                 "excluded_total": 1, "vanished": 0 },
  // When texrun could not finish (exit code 2 or 3):
  "error": { "stage": "probe", "kind": "unavailable", "category": "runtime",
             "message": "engine `texlive` is unavailable: ...", "hint": "..." }
}
```

`diagnostics[].severity` is `error`, `warning` or `info`. When TeX stops
because of an error (`-halt-on-error`), the `emergency_stop` diagnostic that
follows it is `info`, so the errors are the problems to fix; the human output
does not show it. A missing package or class has the line of its
`\usepackage` / `\documentclass` only when it can be told for certain.

BibTeX problems are reported from its `.blg` logs and latexmk's output: a
syntax error or repeated entry in a `.bib` file is `bibtex_error` with the
`.bib` file and the line BibTeX reports (only the file when BibTeX read on
past the mistake, e.g. an entry that is not closed; the message says where
it noticed); a database or style BibTeX cannot
open is `missing_file`; a `.bib` named by `\bibliography` that does not exist
(latexmk then does not run BibTeX) is a `missing_file` warning; an entry not
in the databases is an `undefined_citation` warning from BibTeX next to
LaTeX's. Like latexmk, a document without `\cite` yet (`I found no
\citation commands`) gives only a warning. `bibtex_failed` says that BibTeX failed (`info` after its errors,
`error` when there are none to show). The `.blg` itself is not an artifact.

A compile that texrun stopped at a resource limit fails (exit 1) with a
`resource_limit` error diagnostic that says which: the output size (per file
or in total), the CPU time of a process, the memory, or the number of
processes (docs/security.md §3.2, §3.10). The limits are not options; a
limit reached before a timeout is reported next to `timed_out` too.
`resource_limits` says which layers were in place: `rlimits` (per-process
limits set before latexmk starts) and `cgroup` (Linux, see `--cgroup`; with
`--backend container` the container's own cgroup), with `notes` on a
missing layer. With `--backend container`, `engine.name` is
`texlive-container` and `engine.version` also names the runtime and the
image.

Check `error` first, then `outcome` (or just `texrun_exit_code`). `exit` is
the latexmk process status and is informational only. `error` can appear
together with an `outcome`, e.g. when the compile succeeded but its output
could not be copied: `artifacts` then lists what was copied and
`artifacts_not_copied` the rest. New fields and enum values may be added
without changing `schema_version`.

`error.stage` is one of `args`, `project`, `output`, `probe`, `workspace`,
`compile`, `collect`, `setup`. `error.kind` is a stable `snake_case` code:

- from the CLI: `usage`, `invalid_preview_options`, `non_utf8_path`,
  `unsafe_root`, `unsafe_output_path` (stage `output`), `io` (stage
  `output`), `signal_setup`, `unsupported` (stage `setup`: `--cgroup
  required` without a usable cgroup);
- from the engine (stages `probe`, `compile`): `unavailable`, `spawn`, `io`,
  `invalid_request`, `unsupported`;
- from the workspace (stages `project`, `workspace`, `collect`):
  `root_not_directory`, `invalid_entrypoint`, `entrypoint_not_found`,
  `entrypoint_outside_root`, `entrypoint_not_file`, `entrypoint_excluded`,
  `invalid_request`, `symlink_outside_root`, `limit_exceeded`,
  `input_changed`, `artifact_missing`, `artifact_not_file`, `output_exists`,
  `unsafe_output_path`, `io`.

`notes[].kind` is one of `parent_directory_input` (a file above the
entrypoint's directory was not found; `--root` does not help),
`broad_project_root` (an explicit `--root` is `$HOME` or a temporary
directory) and `output_contains_entrypoint` (the output directory contains
the entrypoint, so it is not left out of the workspace and earlier outputs
are copied into it).

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The document compiled and a PDF was produced |
| 1 | The document failed to compile (see the diagnostics), also when a resource limit stopped it (`resource_limit`) |
| 2 | Usage or input error: invalid arguments, entrypoint not found or outside `--root`, project rejected (symlink leaving the root, input limits, unsafe root) |
| 3 | Runtime error: latexmk missing or unusable (with `--backend container`: no usable container runtime or engine image), I/O errors, artifacts could not be copied, `--cgroup required` without a usable cgroup |
| 4 | The compile timed out |
| 130 | Interrupted by SIGINT (Ctrl-C); 143 for SIGTERM, 129 for SIGHUP |

A signal gives 128+N even if it arrives after the compile finished (e.g.
while previews are rendered; the JSON then still shows `outcome` and a
`cancelled` preview notice). Otherwise page previews never change the exit
code; problems with them are reported as preview notices. On Ctrl-C, texrun stops latexmk (its whole process group),
removes the workspace and then exits.

### Engine backends

| | `--backend host` (default) | `--backend container` |
| --- | --- | --- |
| TeX runs | on the host, as your user | in a container of the engine image (Docker or Podman), as a non-root user without capabilities |
| Page previews (MuPDF / Poppler) | the host's tools, as your user | the image's tools, in a container of their own that sees only a copy of the PDF |
| Shell escape off, texrun rc, environment allowlist, kpathsea paranoid mode, timeout and limits | yes | yes (the same settings) |
| Host files TeX can reach | whatever kpathsea's paranoid mode does not refuse by name (e.g. the TeX Live tree, font lookups, pdfTeX's file embedding primitives) | only the workspace (read-only, except the output directory) and the image's own read-only TeX Live tree |
| Network | not blocked | none (`--network none`), for the compile and the previews |
| Memory / processes / CPUs of the whole compile | only with a delegated cgroup (`--cgroup`) | always (the container's cgroup) |
| Needs | TeX Live + latexmk on the host | Docker 20.10+ (running as root) or Podman 4+ (rootless: cgroup v2 with the memory, pids and cpu controllers delegated to your user), and the engine image |

Rootless Podman is supported and tested in CI; texrun keeps your uid in the
container (`--userns keep-id`), so the output belongs to you. Rootless Docker
(`dockerd-rootless`) is detected and refused: its containers cannot write the
output directory as a non-root user (with `--container-runtime auto`, texrun
then tries Podman). Without the delegated cgroup controllers, rootless
Podman cannot enforce the container limits, and texrun refuses it too
(exit 3, with the reason).

Use `--backend container` for documents you do not trust. The default stays
`host` because the container backend needs a container runtime and the image.
Pull the image of your texrun version once ([Engine image](#engine-image)):

```bash
docker pull ghcr.io/nishide-dev/texrun-engine:0.1.0   # the version of `texrun --version`
texrun compile --backend container main.tex
```

The runtime must be local (a Unix socket; Docker Desktop and OrbStack are).
If it does not apply every restriction texrun asks for (for example a memory
limit the kernel does not support), texrun refuses to start the container
(exit 3) instead of running TeX with fewer restrictions.

With `--backend container`, page previews are rendered by the image's
MuPDF / Poppler in one more container per compile, with the same
restrictions and the preview limits; the host's preview tools are not used
(and need not be installed). See [docs/security.md](docs/security.md) §2
and §4 for exactly what each backend guarantees.

### Engine image

Every release publishes the engine image (`docker/engine/Dockerfile`) for
`linux/amd64` and `linux/arm64` as
`ghcr.io/nishide-dev/texrun-engine:<version>`, where `<version>` is the
texrun version without the `v` (`texrun --version`). That image is the
default of `--container-image`, so the container backend works without
cloning this repository:

```bash
cargo install --locked --git https://github.com/nishide-dev/texrun --tag v0.1.0 texrun
docker pull ghcr.io/nishide-dev/texrun-engine:0.1.0
texrun compile --backend container main.tex
```

- texrun never pulls (`--pull never`): downloading the image is always a
  separate, explicit step. Without it, `--backend container` fails with
  exit 3 and a hint.
- A published version is never overwritten. To pin exactly what you
  verified, pass the digest the release workflow printed:
  `--container-image ghcr.io/nishide-dev/texrun-engine@sha256:<digest>`.
- `engine.version` names the image, its ID and its version label, e.g.
  `latexmk 4.86 (docker 29.4.0, image ghcr.io/nishide-dev/texrun-engine:0.1.0
  3681cf4e3444, image version 0.1.0)`. It says `image version X, not Y of
  texrun` when the image belongs to another texrun version, and `image
  without a version label` for a local build.
- The image carries an SBOM and SLSA provenance (buildx attestations) and a
  signed GitHub artifact attestation:
  `gh attestation verify oci://ghcr.io/nishide-dev/texrun-engine:0.1.0 -R nishide-dev/texrun`.
- To build it yourself instead, from a checkout of the same version:
  `docker build -t ghcr.io/nishide-dev/texrun-engine:0.1.0 docker/engine`
  (or any name, with `--container-image`).

Licensing of the image: texrun itself is MIT, but the image contains no
texrun code. It is `debian:trixie-slim` with unmodified Debian packages,
under their own licenses:

| Packages | License |
| --- | --- |
| TeX Live macro packages (`texlive-base`, `texlive-latex-base`, `texlive-latex-recommended`, their dependencies) | free software licenses, mostly the LaTeX Project Public License |
| TeX Live programs (`texlive-binaries`: pdfTeX, BibTeX, makeindex, kpathsea, ...) | GPL and other free software licenses |
| `latexmk` | GPL-2.0-or-later |
| `coreutils` (`timeout`), `util-linux` (`prlimit`), the Debian base system | GPL and other free software licenses |
| `mupdf-tools` (MuPDF), for page previews | AGPL-3.0-or-later |
| `poppler-utils` (Poppler), for page previews | GPL-2.0-only or GPL-3.0-only |

- The license of every package is kept in the image, at
  `/usr/share/doc/<package>/copyright`; the SBOM lists every package and its
  version.
- The complete corresponding source accompanies every published image: the
  GitHub release of the same tag has `texrun-engine-<version>-sources.tar`,
  the Debian source packages (`.dsc`, `.orig.tar.*`, `.debian.tar.*`) of
  every package in the image, for both platforms, at exactly the installed
  versions (`packages.txt` and `SHA256SUMS` are inside). The release
  workflow fetches it when it publishes the image; `dpkg-source -x
  <package>.dsc` unpacks one.
- texrun starts these tools as separate processes and does not link them.
  MuPDF is shipped unmodified, so the AGPL's network clause (for modified
  versions) does not add anything beyond its source requirement.
- If you redistribute the image (for example, mirror it to another
  registry), the GPL / AGPL obligations for the binaries in it apply to you
  as well.

## System requirements

- **OS:** Linux or macOS. Windows is not supported.
- **Rust:** 1.98.1. `rust-toolchain.toml` pins the version, so `rustup` installs
  and selects it automatically.
- **TeX Live + latexmk:** required to compile documents with `--backend host`.
  For development, the Docker-based environment (see below) is recommended
  instead of installing TeX Live on the host.
- **Container runtime (optional):** Docker 20.10+ (including Docker Desktop
  and OrbStack on macOS) or Podman 4+, and the engine image
  (`ghcr.io/nishide-dev/texrun-engine:<version>`, or a local build of
  `docker/engine/Dockerfile`; see [Engine image](#engine-image)), for
  `--backend container`. Docker and rootless Podman are tested in CI;
  rootless Docker is not supported.
- **Preview tool:** `mutool` (MuPDF) or `pdfinfo` + `pdftoppm` (Poppler), for
  page previews with `--backend host` (`--backend container` uses the
  image's). MuPDF is used when both are installed; without either,
  compiling still works and the result says that previews were skipped.
  - Licensing: MuPDF is AGPL and Poppler is GPL. texrun only starts an
    installed binary as a separate process; it neither links nor ships them.
    To avoid MuPDF entirely, select the Poppler backend
    (`--preview-backend poppler`, or `BackendChoice::Poppler` in the library) or do not install
    `mutool`.
  - The development Docker image below and the engine image
    (`docker/engine`) contain `mupdf-tools` and `poppler-utils`. The
    published engine image is distributed under those packages' terms, with
    their source (see [Engine image](#engine-image)); the development image
    is not published.

## Local development

Run the quality gates before opening a pull request (the fast CI jobs run the
same checks):

```bash
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI runs the tests with [cargo-nextest](https://nexte.st/) and also checks the
dependency policy in `deny.toml`. If you add or update dependencies, run:

```bash
cargo deny check
```

A Docker-based environment with the Rust toolchain, TeX Live, latexmk and a
preview tool is provided for running commands that need TeX, for example:

```bash
docker compose run --rm -e TEXRUN_REQUIRE_TEXLIVE=1 -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 \
  dev cargo test --workspace --all-features
```

Without TeX Live, tests that need it are skipped and reported as `SKIPPED` on
stderr; the CI `integration` job runs them inside this image.

See [docs/development.md](docs/development.md) for setup details.

## Security model

TeX can read files and, if enabled, run external commands, so texrun treats
documents as untrusted input. texrun disables shell escape, runs latexmk with a
texrun-managed configuration, restricts TeX file access to the workspace where
kpathsea allows it, passes a minimal environment and enforces a timeout and
output limits. Running TeX on the host (`--backend host`) is **not** a
complete sandbox, though: parts of the host such as the TeX Live tree remain
readable, and network access is not blocked. `--backend container` adds an
OS-level boundary: TeX then sees only the workspace, has no network and runs
without privileges in a read-only container, and so do the preview tools
([Engine backends](#engine-backends)).

See [docs/security.md](docs/security.md) for the trust boundary, guarantees,
limitations and execution limits (Japanese), and [SECURITY.md](SECURITY.md) for
reporting vulnerabilities.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development flow, commit and
pull request conventions.

## License

texrun is licensed under the [MIT License](LICENSE).
