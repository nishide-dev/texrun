---
name: texrun
description: Compiles LaTeX/TeX documents to PDF with the texrun CLI and reads its JSON result to fix compile errors at their file and line and to check page layout from PNG previews. Use when asked to compile, build or typeset a .tex file, fix LaTeX or BibTeX errors and warnings (undefined control sequence, missing package or file, undefined reference or citation, overfull box), or check how the pages of a LaTeX document look. Runs untrusted documents in an isolated container.
---

# Compiling TeX with texrun

texrun runs latexmk on a temporary copy of the project and prints one JSON
document: the outcome, diagnostics with a `kind`, `file` and `line`, the PDF,
and PNG previews of the pages. Use it instead of running latexmk or pdflatex
directly.

## Before you start

1. Run `texrun --version`. If the command is missing, do not fall back to
   latexmk; tell the user how to install texrun (see
   [reference/setup.md](reference/setup.md)) and stop.
2. Pick the backend:
   - **Untrusted document** (downloaded, sent by someone else, or generated
     from untrusted input): always add `--backend container`. TeX then runs
     in a container without network access that sees only the project.
   - **The user's own document**: the default (`--backend host`) is fine.
   - If the host backend fails with `error.kind` `unavailable` (no latexmk),
     retry with `--backend container`; if that fails too, see
     [reference/setup.md](reference/setup.md).

## Compile and fix loop

Copy this checklist and track progress:

```text
- [ ] Compile with --json and read the summary
- [ ] If `error` is present: fix the invocation or environment, not the document
- [ ] If outcome is failed: fix the first error diagnostic, then recompile
- [ ] Repeat until outcome is succeeded (stop after 5 rounds without progress)
- [ ] Review warnings that matter (undefined references/citations, overfull boxes)
- [ ] Check the page previews
```

**1. Compile.** Pass the main `.tex` file (the one with `\documentclass`):

```bash
texrun compile --json --backend container paper/main.tex | python3 scripts/texrun_summary.py
```

`scripts/texrun_summary.py` (in this skill's directory; Python 3 standard
library only) prints the outcome, the error diagnostics as absolute
`path:line: kind: message`, the first warnings (`--all` for every one), the
PDF and the preview image paths. To read the
JSON yourself, drop the pipe; stdout is always exactly one JSON document,
even on failure. Field reference: [reference/json.md](reference/json.md).

**2. Check `error` first.** If the JSON has `error`, texrun itself could not
finish (exit 2 or 3): a wrong path or option, a missing tool or image, or an
output problem. Read `error.message` and `error.hint`, and fix that; the
document may be fine. See the error kinds in
[reference/errors.md](reference/errors.md).

**3. Then `outcome`:**

- `succeeded`: a PDF was produced. Go to step 5.
- `failed`: fix the diagnostics (step 4).
- `timed_out`: look for a loop in the document's macros. Raise the limit
  only if the document is just large: `--timeout 3m`.
- `cancelled`: the run was interrupted; compile again.

**4. Fix errors.** Look at `diagnostics` with `severity` `error`:

- The file to open is `project.root` + `/` + `file`; `line` is 1-based.
- TeX stops at the first error, so a run usually reports one error. Fix it
  and recompile; the next error appears in the next run. Ignore the `info`
  diagnostic `emergency_stop`: it only says TeX stopped.
- `raw_excerpt` holds the log lines; its `l.N` line shows the text TeX was
  reading.
- A diagnostic without `file` comes from an installed package or class; the
  cause is usually the document's own use of it just before.
- If `failed` comes with no `error` diagnostic, texrun did not recognize the
  problem: read the log (`output_dir` + `/` + the path of the `artifacts[]`
  entry of kind `log`) and look for the first line starting with `!` or
  `./file.tex:N:`.
- Fix the cause; do not silence an error (for example by defining an unknown
  macro as empty) unless that is clearly what the author meant.
- Fixes by `kind`: [reference/errors.md](reference/errors.md).

Recompile after each fix. If the same error stays at the same place after
two attempts, or there is no progress after 5 rounds, stop and report the
diagnostic to the user.

**5. Review warnings.** After `succeeded`, latexmk has already rerun TeX, so
a remaining `undefined_reference` or `undefined_citation` warning means a
missing `\label` or bibliography entry. `overfull_box` means text sticks into
the margin: check the page preview.

## Check the pages

After a successful compile, `preview.pages[]` lists the PNG images, by
default of the first 20 pages:

1. The image is `output_dir` + `/` + `preview.pages[].path` (for example
   `texrun-out/preview/page-001.png`); `preview.pages[].page` is the page
   number. The summary script prints the absolute paths.
2. Open the images you need and look at the layout: figures and tables
   running into the margin, overfull lines, unexpected blank pages, missing
   glyphs, wrong page size.
3. For other pages, compile again with `--pages 5-8`; for finer detail,
   `--preview-dpi 200`.

If `preview.status` is `partial` or `skipped`, `preview.notices` says why
(for example no preview tool on the host). Previews never change the exit
code. Use only the paths in the current JSON: files from earlier runs stay in
the output directory and may be stale.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Compiled; PDF produced |
| 1 | The document failed to compile (see `diagnostics`) |
| 2 | Usage or input error (see `error`) |
| 3 | Runtime error: latexmk, container runtime or engine image missing; I/O error (see `error`) |
| 4 | Timed out |
| 130 | Interrupted (128 + signal number) |

With `--json`, the code is also in `texrun_exit_code`, which survives a pipe.

## Options

| Option | Use |
| --- | --- |
| `--json` | One JSON document on stdout; always use it |
| `--backend container` | Isolated container; use for untrusted documents |
| `--output <DIR>` | Output directory (default `texrun-out/` next to the entrypoint) |
| `--root <DIR>` | Project root to copy (default: the entrypoint's directory) |
| `--timeout <DURATION>` | Wall-clock limit, e.g. `90s`, `3m` (default 60s) |
| `--pages <RANGE>` | Pages to preview: `N`, `N-M`, `N-` or `-M` |
| `--preview-dpi <DPI>` | Preview resolution (default 144) |
| `--no-preview` | Skip previews (faster while fixing errors) |

`texrun compile --help` lists the rest.

## Project layout rules

- TeX cannot read files above the entrypoint's directory
  (`\input{../macros}`); keep the main file at the project root. The note
  `parent_directory_input` in `notes` points this out.
- texrun never runs a `latexmkrc` and does not allow shell escape, so
  packages such as `minted` that need `-shell-escape` do not work.
- texrun does not install LaTeX packages: a missing package is a
  `missing_file` error to report to the user (or to replace with an
  available package).
