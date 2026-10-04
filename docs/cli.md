# CLI reference

This document describes `texrun compile` in detail: its options, what goes
into the workspace, the human and JSON output, the error and note codes, the
exit codes and the two engine backends. For an overview, see the
[README](../README.md); `texrun compile --help` prints a summary of the
options and exit codes.

## `texrun compile`

```text
texrun compile [OPTIONS] <ENTRYPOINT>
```

texrun copies the project into a temporary workspace, runs latexmk there with
a timeout, renders PNG previews of the first pages after a successful compile,
and copies the PDF, the log and the previews to the output directory.

```bash
# Human-readable output
texrun compile main.tex

# One JSON document on stdout (for tools and AI agents)
texrun compile --json main.tex

# Run TeX in a hardened container (recommended for untrusted documents)
texrun compile --backend container main.tex
```

### Options

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
| `--backend <BACKEND>` | `host` | Where TeX runs: `host` (the host's latexmk, as your user) or `container` (in a hardened container built from the engine image; see [Engine backends](#engine-backends)) |
| `--container-runtime <RUNTIME>` | `auto` | With `--backend container`: `auto` (Docker if installed and running, otherwise Podman), `docker` or `podman`. Rootless Docker is not supported (use rootless Podman); see [Engine backends](#engine-backends) |
| `--container-image <IMAGE>` | `ghcr.io/nishide-dev/texrun-engine:<version>` | With `--backend container`: the engine image; must exist locally, texrun never pulls (see [engine-image.md](engine-image.md)) |
| `--cgroup <MODE>` | `auto` | Linux: run latexmk and the preview tools in cgroups of their own (memory, processes, CPU). `auto` uses a delegated cgroup if there is one (e.g. `systemd-run --user --scope -p Delegate=yes texrun ...`), otherwise only the per-process limits apply (see `resource_limits`); `required` fails with exit 3 instead; `off` never uses one |

### The workspace

The workspace never contains VCS metadata, `texrun-out/`, `.texrun/`,
precompiled formats or tool configuration such as `latexmkrc` (texrun never
runs it; a warning is printed when one is left out). An `--output` directory
inside the project is left out too, at any depth (reported with reason
`excluded_path`; any other files kept there are not copied either), unless it
contains the entrypoint (also through a symlink): then it is copied with the
project, including the output of earlier runs, and an info note
`output_contains_entrypoint` says so.

TeX cannot read files above the entrypoint's directory (`\input{../x}`); keep
the entrypoint in the project root.

### Page previews

After a successful compile, texrun renders PNG previews of the first pages
(`--pages`, `--preview-dpi`). With `--backend host` it uses the host's
`mutool` (MuPDF) or `pdfinfo` + `pdftoppm` (Poppler); MuPDF is used when both
are installed. Without either, compiling still works and the result says that
previews were skipped. With `--backend container`, the image's tools are used
instead (see [Engine backends](#engine-backends)).

MuPDF is licensed under the AGPL and Poppler under the GPL. texrun only starts
an installed binary as a separate process; it neither links nor ships them. To
avoid MuPDF entirely, select the Poppler backend (`--preview-backend poppler`,
or `BackendChoice::Poppler` in the library) or do not install `mutool`.

Problems with previews never change the exit code (except a signal, see
[Exit codes](#exit-codes)); they are reported as preview notices.

## Output

- Without `--json`, stdout has the result: diagnostics as
  `file:line: error: message` (errors, then warnings, identical ones merged,
  at most 20 each), a status line with the elapsed time, and the paths of the
  PDF (or the log on failure) and previews. Warnings and errors of texrun
  itself go to stderr. Control, bidi and zero-width characters in file names
  and messages are shown escaped (`\u{202E}`).
- With `--json`, stdout contains exactly one JSON document, even on compile
  failure, timeout and runtime errors (and for usage errors when `--json` is
  on the command line). Strings are not escaped beyond JSON. stderr may still
  carry human-readable warnings.

### JSON document

```jsonc
{
  "schema_version": 1,
  "texrun_exit_code": 0,             // the exit code of texrun itself (see Exit codes)
  // When the compile ran to an outcome:
  "outcome": "succeeded",            // failed | timed_out | cancelled
  "engine": { "name": "texlive", "version": "latexmk 4.86" },
  "exit": { "code": 0 },             // how the latexmk process ended, e.g. { "signal": 9 }
  "elapsed_ms": 812,
  "diagnostics": [
    { "severity": "error", "kind": "undefined_control_sequence",
      "message": "Undefined control sequence \\foo", "file": "chapters/intro.tex", "line": 3,
      "raw_excerpt": "./chapters/intro.tex:3: Undefined control sequence.\n..." }
  ],
  "artifacts": [
    { "kind": "pdf", "path": "main.pdf", "size_bytes": 11290 },
    { "kind": "log", "path": "main.log", "size_bytes": 2800 },
    { "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 50212 }
  ],
  // Which OS-level limits were in place for latexmk (docs/security.md §3.10):
  "resource_limits": { "rlimits": true, "cgroup": false,
                       "notes": [ "cgroup: no delegated cgroup: ..." ] },
  // After a successful compile, unless --no-preview:
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

Check `error` first, then `outcome` (or just `texrun_exit_code`). `exit` is
the latexmk process status and is informational only. `error` can appear
together with an `outcome`, e.g. when the compile succeeded but its output
could not be copied: `artifacts` then lists what was copied and
`artifacts_not_copied` the rest. New fields and enum values may be added
without changing `schema_version`.

With `--backend container`, `engine.name` is `texlive-container` and
`engine.version` also names the runtime and the image (see
[engine-image.md](engine-image.md#versions-and-pinning)).

### Diagnostics

`diagnostics[].severity` is `error`, `warning` or `info`. When TeX stops
because of an error (`-halt-on-error`), the `emergency_stop` diagnostic that
follows it is `info`, so the errors are the problems to fix; the human output
does not show it. A missing package or class has the line of its
`\usepackage` / `\documentclass` only when it can be told for certain.

BibTeX problems are reported from its `.blg` logs and latexmk's output:

- A syntax error or repeated entry in a `.bib` file is `bibtex_error` with the
  `.bib` file and the line BibTeX reports (only the file when BibTeX read on
  past the mistake, e.g. an entry that is not closed; the message says where
  it noticed).
- A database or style BibTeX cannot open is `missing_file`; a `.bib` named by
  `\bibliography` that does not exist (latexmk then does not run BibTeX) is a
  `missing_file` warning.
- An entry not in the databases is an `undefined_citation` warning from
  BibTeX next to LaTeX's. Like latexmk, a document without `\cite` yet
  (`I found no \citation commands`) gives only a warning.
- `bibtex_failed` says that BibTeX failed (`info` after its errors, `error`
  when there are none to show).

The `.blg` itself is not an artifact.

### Resource limits

A compile that texrun stopped at a resource limit fails (exit 1) with a
`resource_limit` error diagnostic that says which: the output size (per file
or in total), the CPU time of a process, the memory, or the number of
processes ([security.md](security.md) §3.2, §3.10). The limits are not
options; a limit reached before a timeout is reported next to `timed_out` too.

`resource_limits` says which layers were in place: `rlimits` (per-process
limits set before latexmk starts) and `cgroup` (Linux, see `--cgroup`; with
`--backend container` the container's own cgroup), with `notes` on a missing
layer.

### Error kinds

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

### Note kinds

`notes[].kind` is one of:

- `parent_directory_input`: a file above the entrypoint's directory was not
  found; `--root` does not help.
- `broad_project_root`: an explicit `--root` is `$HOME` or a temporary
  directory.
- `output_contains_entrypoint`: the output directory contains the
  entrypoint, so it is not left out of the workspace and earlier outputs are
  copied into it.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The document compiled and a PDF was produced |
| 1 | The document failed to compile (see the diagnostics), including when a resource limit stopped it (`resource_limit`) |
| 2 | Usage or input error: invalid arguments, entrypoint not found or outside `--root`, project rejected (symlink leaving the root, input limits, unsafe root) |
| 3 | Runtime error: latexmk missing or unusable (with `--backend container`: no usable container runtime or engine image), I/O errors, artifacts could not be copied, `--cgroup required` without a usable cgroup |
| 4 | The compile timed out |
| 130 | Interrupted by SIGINT (Ctrl-C); 143 for SIGTERM, 129 for SIGHUP |

A signal gives 128+N even if it arrives after the compile finished (e.g.
while previews are rendered; the JSON then still shows `outcome` and a
`cancelled` preview notice). Otherwise page previews never change the exit
code. On Ctrl-C, texrun stops latexmk (its whole process group), removes the
workspace and then exits.

## Engine backends

| | `--backend host` (default) | `--backend container` |
| --- | --- | --- |
| TeX runs | on the host, as your user | in a container built from the engine image (Docker or Podman), as a non-root user without capabilities |
| Page previews (MuPDF / Poppler) | the host's tools, as your user | the image's tools, in a container of their own that sees only a copy of the PDF |
| Shell escape off, texrun rc, environment allowlist, kpathsea paranoid mode, timeout and limits | yes | yes (the same settings) |
| Host files TeX can reach | whatever kpathsea's paranoid mode does not refuse by name (e.g. the TeX Live tree, font lookups, pdfTeX's file embedding primitives) | only the workspace (read-only, except the output directory) and the image's own read-only TeX Live tree |
| Network | not blocked | none (`--network none`), for the compile and the previews |
| Memory / processes / CPUs of the whole compile | only with a delegated cgroup (`--cgroup`) | always (the container's cgroup) |
| Needs | TeX Live + latexmk on the host | Docker 20.10+ (running as root) or Podman 4+ (rootless: cgroup v2 with the memory, pids and cpu controllers delegated to your user), and the engine image |

Use `--backend container` for documents you do not trust. The default stays
`host` because the container backend needs a container runtime and the image.

### Container runtimes

- Rootless Podman is supported and tested in CI; texrun keeps your uid in the
  container (`--userns keep-id`), so the output belongs to you. Without the
  delegated cgroup controllers, rootless Podman cannot enforce the container
  limits, and texrun refuses it (exit 3, with the reason).
- Rootless Docker (`dockerd-rootless`) is detected and refused: its
  containers cannot write the output directory as a non-root user (with
  `--container-runtime auto`, texrun then tries Podman).
- The runtime must be local (a Unix socket; Docker Desktop and OrbStack are).
  If it does not apply every restriction texrun asks for (for example a
  memory limit the kernel does not support), texrun refuses to start the
  container (exit 3) instead of running TeX with fewer restrictions.
- texrun never pulls the image; see [engine-image.md](engine-image.md).

With `--backend container`, page previews are rendered by the image's MuPDF /
Poppler in one more container per compile, with the same restrictions and the
preview limits; the host's preview tools are not used (and need not be
installed). See [security.md](security.md) §2 and §4 for exactly what each
backend guarantees.
