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
  `output`), `signal_setup`;
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
| 1 | The document failed to compile (see the diagnostics) |
| 2 | Usage or input error: invalid arguments, entrypoint not found or outside `--root`, project rejected (symlink leaving the root, input limits, unsafe root) |
| 3 | Runtime error: latexmk missing or unusable, I/O errors, artifacts could not be copied |
| 4 | The compile timed out |
| 130 | Interrupted by SIGINT (Ctrl-C); 143 for SIGTERM, 129 for SIGHUP |

A signal gives 128+N even if it arrives after the compile finished (e.g.
while previews are rendered; the JSON then still shows `outcome` and a
`cancelled` preview notice). Otherwise page previews never change the exit
code; problems with them are reported as preview notices. On Ctrl-C, texrun stops latexmk (its whole process group),
removes the workspace and then exits.

## System requirements

- **OS:** Linux or macOS. Windows is not supported.
- **Rust:** 1.98.1. `rust-toolchain.toml` pins the version, so `rustup` installs
  and selects it automatically.
- **TeX Live + latexmk:** required to compile documents. For development, the
  Docker-based environment (see below) is recommended instead of installing
  TeX Live on the host.
- **Preview tool:** `mutool` (MuPDF) or `pdfinfo` + `pdftoppm` (Poppler), for
  page previews. MuPDF is used when both are installed; without either,
  compiling still works and the result says that previews were skipped.
  - Licensing: MuPDF is AGPL and Poppler is GPL. texrun only starts an
    installed binary as a separate process; it neither links nor ships them.
    To avoid MuPDF entirely, select the Poppler backend
    (`--preview-backend poppler`, or `BackendChoice::Poppler` in the library) or do not install
    `mutool`.
  - The development Docker image below contains `mupdf-tools` and
    `poppler-utils`. If that image is ever distributed, the AGPL / GPL terms
    for distributing those packages apply to the image and must be checked
    separately.

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
output limits. In-process execution is **not** a complete sandbox, though (for
example, parts of the host such as the TeX Live tree remain readable); a
container-based backend is tracked in
[#26](https://github.com/nishide-dev/texrun/issues/26). Network access is not
blocked yet ([#24](https://github.com/nishide-dev/texrun/issues/24)).

See [docs/security.md](docs/security.md) for the trust boundary, guarantees,
limitations and execution limits (Japanese), and [SECURITY.md](SECURITY.md) for
reporting vulnerabilities.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development flow, commit and
pull request conventions.

## License

texrun is licensed under the [MIT License](LICENSE).
