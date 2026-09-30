#!/usr/bin/env bash
# Regenerates the LaTeX log fixtures in `logs/` from the documents in `src/`.
#
# Run inside the dev container (see docs/development.md), from the repo root:
#
#   docker compose run --rm dev crates/texrun-latex-log/tests/fixtures/regenerate.sh
#
# Every document is compiled in a temporary directory (the document root is
# the working directory, as the TeX Live engine #5 does) with the latexmk
# arguments planned for #5:
#
#   latexmk -pdf -norc -interaction=nonstopmode -halt-on-error
#           -file-line-error -no-shell-escape main.tex
#
# and `max_print_line=10000` so TeX does not wrap log lines at 79 columns.
# A few fixtures deliberately deviate to check that the parser degrades
# gracefully:
#
# - `traditional`: without `-file-line-error` (`! ...` + `l.N` form);
# - `wrapped`: without `max_print_line` (TeX's default 79-column wrapping);
# - `rerun`: a single `pdflatex` pass with the same options, because latexmk
#   reruns until the "Rerun to get cross-references right" warning is gone.
#
# The generated logs contain the TeX Live version and paths of the container;
# the tests only check structured results, not the full text.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
src="$here/src"
out="$here/logs"
mkdir -p "$out"

common=(-interaction=nonstopmode -halt-on-error -no-shell-escape)

compile() {
  local name="$1" mode="$2"
  local work
  work="$(mktemp -d)"
  cp -R "$src/$name/." "$work/"
  (
    cd "$work"
    case "$mode" in
      default)
        max_print_line=10000 latexmk -pdf -norc "${common[@]}" -file-line-error main.tex ;;
      traditional)
        max_print_line=10000 latexmk -pdf -norc "${common[@]}" main.tex ;;
      wrapped)
        latexmk -pdf -norc "${common[@]}" -file-line-error main.tex ;;
      single-pass)
        max_print_line=10000 pdflatex "${common[@]}" -file-line-error main.tex ;;
    esac
  ) >/dev/null 2>&1 || true
  cp "$work/main.log" "$out/$name.log"
  rm -rf "$work"
  echo "generated logs/$name.log ($mode)"
}

compile undefined-control-sequence default
compile missing-package default
compile missing-input default
compile package-error default
compile latex-error default
compile multi-file default
compile warnings default
compile traditional traditional
compile wrapped wrapped
compile rerun single-pass
