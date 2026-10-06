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


def location(diag, root):
    """`/abs/file:line` for a diagnostic, or a marker when it has no file."""
    file = diag.get("file")
    if not file:
        return "(no file: installed package or class)"
    path = os.path.join(root, file) if root else file
    line = diag.get("line")
    return f"{path}:{line}" if line is not None else path


def print_diagnostic(diag, root, with_excerpt):
    kind = diag.get("kind", "other")
    message = diag.get("message", "")
    print(f"  {location(diag, root)}: {kind}: {message}")
    excerpt = diag.get("raw_excerpt")
    if with_excerpt and excerpt:
        for line in excerpt.splitlines():
            print(f"      | {line}")


def summarize(report, show_all):
    root = (report.get("project") or {}).get("root", "")
    output_dir = report.get("output_dir", "")
    exit_code = report.get("texrun_exit_code")
    outcome = report.get("outcome", "not compiled")
    print(f"outcome: {outcome} (texrun exit code {exit_code})")

    error = report.get("error")
    if error:
        print(
            f"texrun error: stage {error.get('stage')}, kind {error.get('kind')}: "
            f"{error.get('message', '')}"
        )
        if error.get("hint"):
            print(f"  hint: {error.get('hint')}")

    for note in report.get("notes", []):
        print(f"note: {note.get('kind')}: {note.get('message', '')}")

    diagnostics = report.get("diagnostics", [])
    by_severity = {"error": [], "warning": [], "info": []}
    for diag in diagnostics:
        by_severity.setdefault(diag.get("severity", "info"), []).append(diag)

    errors = by_severity["error"]
    if errors:
        print(f"errors ({len(errors)}), fix the first one and compile again:")
        for diag in errors:
            print_diagnostic(diag, root, with_excerpt=True)

    warnings = by_severity["warning"]
    if warnings:
        shown = warnings if show_all else warnings[:MAX_WARNINGS]
        print(f"warnings ({len(warnings)}):")
        for diag in shown:
            print_diagnostic(diag, root, with_excerpt=False)
        if len(shown) < len(warnings):
            print(f"  ... {len(warnings) - len(shown)} more (use --all)")

    if show_all and by_severity["info"]:
        print(f"info ({len(by_severity['info'])}):")
        for diag in by_severity["info"]:
            print_diagnostic(diag, root, with_excerpt=False)

    def out(path):
        return os.path.join(output_dir, path) if output_dir else path

    for artifact in report.get("artifacts", []):
        if artifact.get("kind") in ("pdf", "log"):
            print(f"{artifact.get('kind')}:{out(artifact.get('path', ''))}")

    preview = report.get("preview")
    if preview:
        pages = preview.get("pages", [])
        page_count = (preview.get("pdf") or {}).get("page_count")
        total = f" of {page_count}" if page_count is not None else ""
        print(f"previews ({preview.get('status')}, {len(pages)}{total} pages):")
        for page in pages:
            print(f"  page {page.get('page')}: {out(page.get('path', ''))}")
        for notice in preview.get("notices", []):
            print(f"  notice: {notice.get('kind')}: {notice.get('message', '')}")

    for artifact in report.get("artifacts_not_copied", []):
        print(f"not copied: {artifact.get('kind')} {artifact.get('path')}")


def main(argv):
    show_all = "--all" in argv
    args = [a for a in argv if a != "--all"]
    if len(args) > 1 or (args and args[0] in ("-h", "--help")):
        print(__doc__.strip())
        return 0 if args and args[0] in ("-h", "--help") else 2
    try:
        if args:
            with open(args[0], encoding="utf-8") as f:
                text = f.read()
        else:
            text = sys.stdin.read()
    except OSError as e:
        print(f"texrun_summary: cannot read {args[0]}: {e}", file=sys.stderr)
        return 2
    try:
        report = json.loads(text)
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
