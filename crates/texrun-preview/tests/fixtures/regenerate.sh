#!/bin/sh
# Regenerates the PDF fixtures in this directory from src/*.tex with plain
# pdfTeX.
#
# Run it inside the development container so the output does not depend on
# the host TeX installation:
#
#   docker compose run --rm dev crates/texrun-preview/tests/fixtures/regenerate.sh
#
# The sources disable dates, the trailer /ID and the pdfTeX banner, so the
# output is byte-for-byte reproducible with the same TeX Live version.
set -eu
cd "$(dirname "$0")"
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
for src in src/*.tex; do
  name=$(basename "$src" .tex)
  pdftex -interaction=nonstopmode -halt-on-error -output-directory="$out" "$src" >/dev/null
  cp "$out/$name.pdf" "$name.pdf"
  echo "wrote $name.pdf ($(wc -c <"$name.pdf") bytes)"
done
