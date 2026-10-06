#!/usr/bin/env python3
"""Summarize the JSON result of `texrun compile --json` for fixing a document.

Usage:
    texrun compile --json main.tex | python3 texrun_summary.py
    python3 texrun_summary.py result.json
    python3 texrun_summary.py --all result.json   # also every warning and info

Prints the outcome, texrun's own error (if any), the diagnostics as absolute
`path:line: kind: message`, and the absolute paths of the PDF and the page
previews. Python 3 standard library only.

Exit status: 0 when a texrun result was read (whatever its outcome), 2 when
the input is not a texrun JSON document.
"""

import json
import os
import sys

# The default output keeps the summary short enough to read in one go; the
# full list is in the JSON (or with --all).
MAX_WARNINGS = 10


def obj(value):
    """`value` if it is a JSON object, else an empty one."""
    return value if isinstance(value, dict) else {}


def objects(value):
    """The JSON objects in `value` if it is a list, else none."""
    return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []


def text(value):
    """A JSON value as text (missing values as an empty string)."""
    return "" if value is None else str(value)


def location(diag, root):
    """`/abs/file:line` for a diagnostic, or a marker when it has no file."""
    file = diag.get("file")
    if not file or not isinstance(file, str):
        return "(no file: installed package or class)"
    path = os.path.join(root, file) if root else file
    line = diag.get("line")
    return f"{path}:{line}" if line is not None else path


def print_diagnostic(diag, root, with_excerpt):
    kind = text(diag.get("kind")) or "other"
    print(f"  {location(diag, root)}: {kind}: {text(diag.get('message'))}")
    excerpt = diag.get("raw_excerpt")
    if with_excerpt and isinstance(excerpt, str):
        for line in excerpt.splitlines():
            print(f"      | {line}")


def summarize(report, show_all):
    root = text(obj(report.get("project")).get("root"))
    output_dir = text(report.get("output_dir"))
    outcome = text(report.get("outcome")) or "not compiled"
    print(f"outcome: {outcome} (texrun exit code {report.get('texrun_exit_code')})")

    error = obj(report.get("error"))
    if error:
        print(
            f"texrun error: stage {text(error.get('stage'))}, kind {text(error.get('kind'))}: "
            f"{text(error.get('message'))}"
        )
        if error.get("hint"):
            print(f"  hint: {text(error.get('hint'))}")

    for note in objects(report.get("notes")):
        print(f"note: {text(note.get('kind'))}: {text(note.get('message'))}")

    errors, warnings, others = [], [], []
    for diag in objects(report.get("diagnostics")):
        severity = diag.get("severity")
        if severity == "error":
            errors.append(diag)
        elif severity == "warning":
            warnings.append(diag)
        else:
            # `info`, and severities added after this script was written.
            others.append(diag)

    if errors:
        print(f"errors ({len(errors)}), fix the first one and compile again:")
        for diag in errors:
            print_diagnostic(diag, root, with_excerpt=True)
    elif outcome == "failed":
        print(
            "errors: none recognized; read the log below and look for the first "
            "line starting with `!` or `./file.tex:N:`"
        )

    if warnings:
        shown = warnings if show_all else warnings[:MAX_WARNINGS]
        print(f"warnings ({len(warnings)}):")
        for diag in shown:
            print_diagnostic(diag, root, with_excerpt=False)
        if len(shown) < len(warnings):
            print(f"  ... {len(warnings) - len(shown)} more (use --all)")

    if show_all and others:
        print(f"info and other ({len(others)}):")
        for diag in others:
            severity = text(diag.get("severity")) or "unknown"
            print(f"  [{severity}]", end="")
            print_diagnostic(diag, root, with_excerpt=False)

    def out(path):
        path = text(path)
        return os.path.join(output_dir, path) if output_dir else path

    for artifact in objects(report.get("artifacts")):
        if artifact.get("kind") in ("pdf", "log"):
            print(f"{artifact.get('kind')}: {out(artifact.get('path'))}")

    preview = obj(report.get("preview"))
    if preview:
        pages = objects(preview.get("pages"))
        page_count = obj(preview.get("pdf")).get("page_count")
        total = f" of {page_count}" if page_count is not None else ""
        print(f"previews ({text(preview.get('status'))}, {len(pages)}{total} pages):")
        for page in pages:
            print(f"  page {page.get('page')}: {out(page.get('path'))}")
        for notice in objects(preview.get("notices")):
            print(f"  notice: {text(notice.get('kind'))}: {text(notice.get('message'))}")

    for artifact in objects(report.get("artifacts_not_copied")):
        print(f"not copied: {text(artifact.get('kind'))} {text(artifact.get('path'))}")


def main(argv):
    # Never fail on characters the terminal's encoding cannot show.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="backslashreplace")
    show_all = "--all" in argv
    args = [a for a in argv if a != "--all"]
    if args and args[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    if len(args) > 1:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    try:
        if args:
            with open(args[0], "rb") as f:
                data = f.read()
        else:
            data = sys.stdin.buffer.read()
    except OSError as e:
        print(f"texrun_summary: cannot read {args[0]}: {e}", file=sys.stderr)
        return 2
    try:
        report = json.loads(data.decode("utf-8", "replace"))
    except json.JSONDecodeError as e:
        print(
            "texrun_summary: the input is not JSON; run `texrun compile --json ...` "
            f"(stdout only, not stderr): {e}",
            file=sys.stderr,
        )
        return 2
    if not isinstance(report, dict) or "schema_version" not in report:
        print(
            "texrun_summary: the input is not a texrun result (no schema_version)",
            file=sys.stderr,
        )
        return 2
    summarize(report, show_all)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
