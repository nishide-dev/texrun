#!/usr/bin/env bash
# Regenerates the LaTeX log fixtures in `logs/` from the documents in `src/`.
#
# Run inside the dev container (see docs/development.md; use `docker compose`
# instead of `docker-compose` if that is what your Docker provides), from the
# repo root:
#
#   docker-compose run --rm dev crates/texrun-latex-log/tests/fixtures/regenerate.sh
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
# - `traditional`, `unusual-names-traditional`,
#   `missing-package-traditional`: without `-file-line-error`
#   (`! ...` + `l.N` form);
# - `wrapped`: without `max_print_line` (TeX's default 79-column wrapping);
# - `rerun`: a single `pdflatex` pass with the same options, because latexmk
#   reruns until the "Rerun to get cross-references right" warning is gone.
# - `pdftex-missing-font`: with `MKTEXPK=0` (as the TeX Live engine sets it),
#   so that a font without a Type 1 version (here `ecrm1000`, cm-super not
#   being used) cannot be made as a bitmap: `!pdfTeX error: ... not found`.
# - `cannot-write`: with `openout_any=p` (as the TeX Live engine), which
#   refuses `\openout` to an absolute path.
# - `interruption`: a single `pdflatex` pass in an endless loop, stopped
#   with SIGINT after 3 seconds (`! Interruption.`).
#
# The BibTeX logs `logs/bibtex-*.blg` come from one `pdflatex` pass (same
# options) followed by `bibtex main` in the document directory (mode
# `bibtex`). latexmk is not used for them: it does not run BibTeX at all
# when a `.bib` file is missing, so there would be no `.blg` to test with.
# `openin_any=p` as in the TeX Live engine.
#
# The generated logs contain the TeX Live version and paths of the container;
# the tests only check structured results, not the full text.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
src="$here/src"
out="$here/logs"
mkdir -p "$out"

common=(-interaction=nonstopmode -halt-on-error -no-shell-escape)

# compile <log name> <mode> [<document dir in src/, default: log name>]
compile() {
  local name="$1" mode="$2" doc="${3:-$1}"
  local work
  work="$(mktemp -d)"
  cp -R "$src/$doc/." "$work/"
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
      no-mktexpk)
        MKTEXPK=0 max_print_line=10000 latexmk -pdf -norc "${common[@]}" -file-line-error main.tex ;;
      paranoid-out)
        openout_any=p max_print_line=10000 latexmk -pdf -norc "${common[@]}" -file-line-error main.tex ;;
      interrupt)
        max_print_line=10000 timeout -s INT 3 pdflatex "${common[@]}" -file-line-error main.tex ;;
      bibtex)
        max_print_line=10000 pdflatex "${common[@]}" -file-line-error main.tex || true
        openin_any=p bibtex main ;;
    esac
  ) >/dev/null 2>&1 || true
  if [[ "$mode" == bibtex ]]; then
    if [[ ! -f "$work/main.blg" ]]; then
      echo "error: $name ($mode): BibTeX produced no main.blg" >&2
      rm -rf "$work"
      return 1
    fi
    cp "$work/main.blg" "$out/$name.blg"
    rm -rf "$work"
    echo "generated logs/$name.blg ($mode)"
    return 0
  fi
  if [[ ! -f "$work/main.log" ]]; then
    echo "error: $name ($mode): TeX produced no main.log" >&2
    rm -rf "$work"
    return 1
  fi
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
compile missing-class default
compile unusual-names default
compile unusual-names-traditional traditional unusual-names
compile unbalanced-parens default
compile missing-package-before-usepackage default
compile missing-package-options default
compile missing-package-list default
compile missing-package-same-line default
compile missing-package-in-sty default
compile missing-package-traditional traditional missing-package-before-usepackage
compile missing-package-indented default
compile pdftex-missing-font no-mktexpk
compile pdftex-missing-image default
compile pdftex-error-ext default
compile capacity-exceeded default
compile cannot-write paranoid-out
compile interruption interrupt
compile bibtex-syntax bibtex
compile bibtex-missing-database bibtex
compile bibtex-missing-entry bibtex
compile bibtex-missing-style bibtex
compile bibtex-multi-database bibtex
compile bibtex-unclosed-entry bibtex
compile bibtex-unclosed-brace bibtex
compile bibtex-unclosed-quote bibtex
compile bibtex-no-citations bibtex
