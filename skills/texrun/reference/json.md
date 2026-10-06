# texrun JSON result

`texrun compile --json` prints exactly one JSON document on stdout, also when
the compile fails or texrun hits an error. Fields that do not apply are left
out. New fields and enum values may appear without a `schema_version`
change: ignore unknown fields and treat unknown `kind` values like `other`.

## Contents

- Reading order
- Top level
- Diagnostics
- Artifacts and previews
- Project and workspace
- Example

## Reading order

1. `error` present → texrun could not finish; read `error.message` and
   `error.hint` (exit 2 or 3).
2. Otherwise `outcome`: `succeeded`, `failed`, `timed_out` or `cancelled`.
3. `diagnostics` → what to fix; `preview.pages` → what the pages look like.

`error` can come together with an `outcome`, for example when the document
compiled but the output could not be copied (`artifacts_not_copied` then
lists what is missing from `output_dir`).

## Top level

| Field | Meaning |
| --- | --- |
| `schema_version` | `1` |
| `texrun_exit_code` | texrun's own exit code (0, 1, 2, 3, 4, 128+N) |
| `outcome` | `succeeded`, `failed`, `timed_out` or `cancelled`; absent when texrun stopped before compiling |
| `error.stage` | Where texrun stopped: `args`, `project`, `output`, `probe`, `workspace`, `compile`, `collect` or `setup` |
| `error.kind` | Stable code, see errors.md |
| `error.category` | `usage`, `input` (exit 2) or `runtime` (exit 3) |
| `error.message` | Human-readable message with its causes |
| `error.hint` | A suggested fix, when texrun has one |
| `notes[].kind` | Advice about the run: `parent_directory_input`, `broad_project_root`, `output_contains_entrypoint` |
| `notes[].message` | The advice as text |
| `engine.name` | `texlive` (host) or `texlive-container` |
| `engine.version` | latexmk version; with the container backend also the runtime and image |
| `exit` | How the latexmk process ended (`code` or `signal`); informational only, use `texrun_exit_code` |
| `elapsed_ms` | Duration of the compile |
| `resource_limits.rlimits` | Per-process limits were set |
| `resource_limits.cgroup` | A cgroup limited the whole compile (always with `--backend container`) |
| `resource_limits.notes` | Why a layer is missing |

## Diagnostics

| Field | Meaning |
| --- | --- |
| `diagnostics[].severity` | `error`, `warning` or `info` |
| `diagnostics[].kind` | Stable classification, see errors.md |
| `diagnostics[].message` | One-paragraph message |
| `diagnostics[].file` | Path relative to `project.root`; absent for files outside the project (installed packages) |
| `diagnostics[].line` | 1-based line in `file`, when known |
| `diagnostics[].raw_excerpt` | The log lines the diagnostic was read from |

Errors come first in the order TeX reported them. After an error, TeX stops
(`emergency_stop`, severity `info`), so fix errors one run at a time.

## Artifacts and previews

All artifact paths are relative to `output_dir`.

| Field | Meaning |
| --- | --- |
| `output_dir` | Absolute output directory (default `texrun-out/` next to the entrypoint) |
| `artifacts[].kind` | `pdf`, `log` or `preview` |
| `artifacts[].path` | e.g. `main.pdf`, `main.log`, `preview/page-001.png` |
| `artifacts[].page` | Page number of a `preview` |
| `artifacts[].size_bytes` | File size |
| `artifacts_not_copied[].path` | Produced but not copied (only with `error`) |
| `preview.status` | `rendered`, `partial` or `skipped` |
| `preview.backend` | `mupdf` or `poppler` |
| `preview.pdf.page_count` | Number of pages in the PDF |
| `preview.pages[].page` | Page number |
| `preview.pages[].path` | PNG path relative to `output_dir` |
| `preview.pages[].width_px` | Image width |
| `preview.pages[].height_px` | Image height |
| `preview.pages[].dpi` | Resolution used (lower for very large pages) |
| `preview.notices[].kind` | Why a page was not rendered or was changed, see errors.md |
| `preview.notices[].message` | The notice as text |
| `preview.notices[].page` | The page concerned, if any |

`preview` is present only after a successful compile without
`--no-preview`. A failed compile has only the log in `artifacts`; a PDF in
`output_dir` from an earlier run is not removed, so trust `artifacts`, not
the directory listing.

## Project and workspace

| Field | Meaning |
| --- | --- |
| `project.root` | Absolute project root; `diagnostics[].file` is relative to it |
| `project.entrypoint` | Entrypoint relative to the root |
| `workspace.excluded[].path` | A file left out of the copy (e.g. `latexmkrc`, `.git`) |
| `workspace.excluded[].reason` | Why, e.g. `tool_config`, `excluded_name` |
| `workspace.kept_path` | The kept workspace (only with `--keep-workspace`) |

## Example

A failed compile (some fields omitted):

```json
{
  "schema_version": 1,
  "texrun_exit_code": 1,
  "outcome": "failed",
  "diagnostics": [
    {
      "severity": "error",
      "kind": "undefined_control_sequence",
      "message": "Undefined control sequence \\foo",
      "file": "main.tex",
      "line": 5,
      "raw_excerpt": "./main.tex:5: Undefined control sequence.\nl.5 Hello \\foo\n               world."
    },
    {
      "severity": "info",
      "kind": "emergency_stop",
      "message": "Fatal error occurred, no output PDF file produced!",
      "file": "main.tex",
      "line": 5
    }
  ],
  "artifacts": [{ "kind": "log", "path": "main.log", "size_bytes": 4696 }],
  "output_dir": "/home/me/paper/texrun-out",
  "project": { "root": "/home/me/paper", "entrypoint": "main.tex" }
}
```

A successful compile adds the PDF and the previews:

```json
{
  "texrun_exit_code": 0,
  "outcome": "succeeded",
  "artifacts": [
    { "kind": "pdf", "path": "main.pdf", "size_bytes": 23576 },
    { "kind": "log", "path": "main.log", "size_bytes": 3014 },
    { "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 10625 }
  ],
  "preview": {
    "status": "rendered",
    "backend": "mupdf",
    "format": "png",
    "pdf": { "page_count": 1, "pages": [ { "page": 1, "width_pt": 595.276, "height_pt": 841.89, "rotation": 0 } ] },
    "pages": [
      { "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 10625,
        "width_px": 1191, "height_px": 1684, "dpi": 144 }
    ],
    "notices": []
  },
  "output_dir": "/home/me/paper/texrun-out"
}
```
