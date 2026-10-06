# Fixing texrun results by kind

## Contents

- Diagnostic kinds (problems in the document)
- Error kinds (texrun could not finish)
- Note kinds (advice about the run)
- Preview notice kinds

## Diagnostic kinds

`diagnostics[].kind`. Fix `error` diagnostics first, one run at a time.

| Kind | Typical cause and fix |
| --- | --- |
| `undefined_control_sequence` | A misspelled macro, or a macro from a package that is not loaded. Fix the spelling or add the `\usepackage` that defines it. The `l.N` line of `raw_excerpt` shows where |
| `missing_file` | `\input`/`\include`/`\includegraphics` of a file that does not exist (check the path relative to the main file, and the extension), or a package/class that is not installed (the message names the `.sty`/`.cls`). texrun cannot install packages: tell the user, or use an installed alternative. Also a `.bib` or `.bst` that BibTeX cannot open |
| `latex_error` | Any other LaTeX or package error: unbalanced braces or environments (`\begin{itemize}` closed by `\end{enumerate}`), math commands outside math mode (`Missing $ inserted`), a misplaced `&` or `\\`, wrong package options. Read `message` and the `l.N` line |
| `emergency_stop` | TeX stopped after an error (severity `info`). Not a problem of its own: fix the error before it |
| `overfull_box` | A line or box is wider than the text: long words or URLs, wide tables or figures. Check the preview; fix with line breaks, `\url`, smaller content or `\resizebox`. Warning only |
| `underfull_box` | Loose spacing, often from `\\` at the end of a paragraph or forced breaks. Usually harmless |
| `undefined_reference` | `\ref`/`\pageref`/`\eqref` to a label that does not exist. latexmk already reran TeX, so the `\label` is missing or misspelled |
| `undefined_citation` | `\cite` key not in the `.bib` files (or no `\bibliography`). Check the key and the `.bib` file |
| `rerun_required` | LaTeX asked for another run; latexmk normally handles it. If it remains, compile again |
| `bibtex_error` | A syntax error or repeated entry in a `.bib` file, at the `file` and `line` BibTeX reports (an unclosed entry may be reported later than where it starts) |
| `bibtex_failed` | BibTeX failed, so the bibliography is incomplete. Fix the `bibtex_error`/`missing_file` diagnostics next to it |
| `resource_limit` | texrun stopped the compile at a limit on output size, CPU time, memory or processes. Look for runaway loops or huge generated output; the limits are not options |
| `other` | Not classified. Read `message` and `raw_excerpt` |

## Error kinds

`error.kind`, present when texrun could not finish. The document may be fine;
`error.hint` often says what to do.

| Kind | Meaning and fix |
| --- | --- |
| `usage` | Invalid command line. Check the options with `texrun compile --help` |
| `invalid_preview_options` | Bad `--pages`, `--preview-dpi` or `--preview-backend` value |
| `non_utf8_path` | A path argument is not valid UTF-8; rename or move the project |
| `unsafe_root` | The project root would be `/`, `$HOME` or a temporary directory. Put the document in its own directory |
| `unsafe_output_path` | The output directory goes through a symlink or a non-directory is in the way. Choose another `--output` |
| `io` | Reading the project or writing the output failed; check permissions and disk space |
| `signal_setup` | texrun could not install its signal handlers; retry |
| `unsupported` | Something required is missing on this host, e.g. `--cgroup required` without a usable cgroup |
| `unavailable` | latexmk is missing on the host, or (with `--backend container`) there is no usable Docker/Podman or the engine image is not pulled. Follow `error.hint`; see setup.md |
| `spawn` | A tool could not be started; reinstall or check PATH |
| `invalid_request` | The request was rejected (e.g. a bad timeout) |
| `root_not_directory` | `--root` is not a directory |
| `invalid_entrypoint` | The entrypoint path cannot be used in the workspace (e.g. forbidden characters in its name); rename the file |
| `entrypoint_not_found` | The `.tex` file does not exist; check the path |
| `entrypoint_outside_root` | The entrypoint is not inside `--root`; pass a root that contains it |
| `entrypoint_not_file` | The entrypoint is a directory or special file |
| `entrypoint_excluded` | The entrypoint is in a directory texrun leaves out (e.g. `texrun-out/`, `.git`) |
| `symlink_outside_root` | A symlink in the project points outside the root; copy the target into the project |
| `limit_exceeded` | The project is too large to copy (files, bytes or depth); compile from a smaller directory |
| `input_changed` | A file changed while it was being copied; compile again |
| `artifact_missing` | An output reported by latexmk is missing; compile again |
| `artifact_not_file` | An output is not a regular file |
| `output_exists` | An output file already exists and could not be replaced |

## Note kinds

`notes[].kind`: advice, not errors.

| Kind | Meaning |
| --- | --- |
| `parent_directory_input` | A file above the entrypoint's directory was not found (`\input{../x}`). TeX cannot read there and `--root` does not help: move the main file to the project root or copy the file into the project |
| `broad_project_root` | `--root` is `$HOME` or a temporary directory; everything under it is copied. Use a narrower root |
| `output_contains_entrypoint` | The output directory contains the main file, so earlier outputs are copied with the project. Use a separate `--output` |

## Preview notice kinds

`preview.notices[].kind`. Previews never change the exit code.

| Kind | Meaning |
| --- | --- |
| `tool_unavailable` | No preview tool (MuPDF `mutool` or Poppler) on the host. Install one, or use `--backend container` |
| `pdf_unreadable` | The PDF could not be opened |
| `page_limit` | Only some pages were selected; use `--pages` for others |
| `page_range_clamped` | `--pages` went past the last page and was shortened |
| `page_range_out_of_bounds` | `--pages` starts after the last page; nothing was rendered |
| `resolution_reduced` | A large page was rendered at a lower DPI |
| `render_failed` | The tool failed on a page |
| `size_limit` | The total image size limit was reached |
| `timed_out` | Rendering hit its time limit |
| `cancelled` | Rendering was interrupted |
| `output_error` | The images could not be written |
| `resource_limits` | The preview tools' limits could not be set up as intended |
| `limit_exceeded` | A preview tool hit its CPU, memory or process limit |
| `other` | Not classified; read the message |
