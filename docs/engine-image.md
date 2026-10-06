# Engine image

`texrun compile --backend container` runs TeX and the page preview tools in
containers built from the engine image. This document describes how the image is
published, how to get and verify it, and the licenses of the software in it.
For what the container backend guarantees, see [security.md](security.md) §4
and the [backend comparison](cli.md#engine-backends).

## Getting the image

Every release publishes the engine image (`docker/engine/Dockerfile`) for
`linux/amd64` and `linux/arm64` as
`ghcr.io/nishide-dev/texrun-engine:<version>`, where `<version>` is the
texrun version without the `v` (`texrun --version`). That image is the
default of `--container-image`, so the container backend works without
cloning this repository:

```bash
cargo install --locked --git https://github.com/nishide-dev/texrun --tag v0.1.1 texrun
docker pull ghcr.io/nishide-dev/texrun-engine:0.1.1
texrun compile --backend container main.tex
```

texrun never pulls (`--pull never`): downloading the image is always a
separate, explicit step. Without it, `--backend container` fails with exit 3
and a hint.

To build the image yourself instead, from a checkout of the same version:

```bash
docker build -t ghcr.io/nishide-dev/texrun-engine:0.1.1 docker/engine
```

Any other name works too, with `--container-image`.

## Versions and pinning

A published version is never overwritten. To pin exactly what you verified,
pass the digest in the release notes of the version (also printed by the
release workflow):

```bash
texrun compile --backend container \
  --container-image ghcr.io/nishide-dev/texrun-engine@sha256:<digest> main.tex
```

`engine.version` in the JSON output names the image, its ID and its version
label, for example:

```text
latexmk 4.86 (docker 29.4.0, image ghcr.io/nishide-dev/texrun-engine:0.1.1 3681cf4e3444, image version 0.1.1)
```

It says `image version X, not Y of texrun` when the image belongs to another
texrun version, and `image without a version label` for a local build.

## Verifying the image

The image carries an SBOM and SLSA provenance (buildx attestations) and a
signed GitHub artifact attestation:

```bash
gh attestation verify oci://ghcr.io/nishide-dev/texrun-engine:0.1.1 -R nishide-dev/texrun
```

## Licenses

texrun itself is MIT-licensed, but the image contains no texrun code. It is
`debian:trixie-slim` with unmodified Debian packages and a few unmodified
TeX Live packages, under their own licenses:

| Packages | License |
| --- | --- |
| TeX Live macro packages (`texlive-base`, `texlive-latex-base`, `texlive-latex-recommended`, their dependencies) | free software licenses, mostly the LaTeX Project Public License |
| TeX Live graphics (`texlive-pictures`: pgf / TikZ, pgfplots, tikz-cd, ...; beamer needs pgf) | mostly the LaTeX Project Public License, some GPL |
| `python3.13` and its dependencies (`python3`, `libpython3.13-*`, `media-types`, `netbase`), dependencies of `texlive-pictures` | Python Software Foundation License and other free software licenses |
| TeX Live fonts (`texlive-fonts-recommended`: the PSNFSS fonts, Times, Helvetica, Courier, Palatino, ...) | mostly GPL with a font exception (the URW Type1 fonts that PSNFSS embeds, `fonts/type1/urw/`), and other free software font licenses |
| `cm-super` / `cm-super-minimal` (Type1 EC / TC fonts, T1 and TS1 encodings) | GPL-2.0-or-later with a font exception |
| `lmodern` / `fonts-lmodern` (Latin Modern) | GUST Font License (an LPPL variant) |
| `pfb2t1c2pfb`, `xfonts-utils`, `xfonts-encodings`, `libfontenc` (dependencies of `cm-super` / `lmodern`) | GPL (`pfb2t1c2pfb`), MIT / X11 |
| TeX Live programs (`texlive-binaries`: pdfTeX, BibTeX, makeindex, kpathsea, ...) | GPL and other free software licenses |
| `latexmk` | GPL-2.0-or-later |
| TeX Live packages that Debian only has in `texlive-latex-extra` / `texlive-fonts-extra` / `texlive-science`, from the frozen TeX Live 2024 repository (in `/usr/local/share/texmf`, see below): `siunitx`, `multirow`, `makecell`, `cleveref`, `algorithmicx`, `algorithm2e`, `ifoddpage`, `wrapfig`, `xurl`, `soul`, `mwe`, `upquote` | LaTeX Project Public License |
| ... from the same repository: `inconsolata` (the Inconsolata Type1 / OpenType fonts and their LaTeX support) | SIL Open Font License 1.1 (fonts; `doc/fonts/inconsolata/OFL.txt` in the tree), Apache-2.0 / LPPL / permissive (the other files) |
| ... from the same repository: `algorithms` (algorithm / algorithmic) | LGPL-2.1 |
| ... from the same repository: `units` (nicefrac), `comment` | GPL / GPL-2.0 |
| ... from the same repository: `enumitem`, `threeparttable`, `placeins`, `relsize` | MIT (`enumitem`), other permissive licenses, public domain |
| `coreutils` (`timeout`), `util-linux` (`prlimit`), the Debian base system | GPL and other free software licenses |
| `mupdf-tools` (MuPDF), for page previews | AGPL-3.0-or-later |
| `poppler-utils` (Poppler), for page previews | GPL-2.0-only or GPL-3.0-only |

- The license of every package is kept in the image, at
  `/usr/share/doc/<package>/copyright`; the SBOM lists every package and its
  version.
- The TeX Live packages that are not from Debian are installed in
  `/usr/local/share/texmf` (TEXMFLOCAL) exactly as TeX Live distributes
  them: the archives of the frozen TeX Live 2024 repository (`tlnet-final`,
  the TeX Live version of Debian trixie), checked against the SHA-256 in
  `docker/engine/texlive-archives.sha256`. Their Debian packages come from
  the `texlive-extra` source package, whose 2.8 GB `.orig.tar.xz` alone
  would be over the 2 GiB limit of the sources release asset below. Each
  package's license is in its `tlpkg/tlpobj/<package>.tlpobj`
  (`catalogue-license`) and in the headers of its files.
- texrun starts these tools as separate processes and does not link them.
  MuPDF is shipped unmodified, so the AGPL's network clause (for modified
  versions) does not add anything beyond its source requirement.
- pdfTeX embeds subsets of these fonts in the PDFs it writes. The font
  exceptions of cm-super and of the URW fonts, the GUST Font License
  (Latin Modern) and the SIL Open Font License (Inconsolata) allow that
  without putting the documents under their licenses.
- If you redistribute the image (for example, mirror it to another
  registry), the GPL / AGPL obligations for the binaries in it apply to you
  as well.

### Source code

The complete corresponding source is provided as a release asset alongside
every published image: the GitHub release of the same tag has
`texrun-engine-<version>-sources.tar`, the Debian source packages (`.dsc`,
`.orig.tar.*`, `.debian.tar.*`) of every package in the image, for both
platforms, at exactly the installed versions (`packages.txt` and
`SHA256SUMS` are inside), and in `texlive/` the TeX Live archives of the
packages that are not from Debian (run files, documentation and, where
TeX Live has them, sources: `<package>.tar.xz`, `<package>.doc.tar.xz`,
`<package>.source.tar.xz`, with their `SHA256SUMS`). The release workflow
(`.github/workflows/engine-image.yml`) fetches it when it publishes the
image; `dpkg-source -x <package>.dsc` unpacks one.

### The development image

The development image (`docker/dev/Dockerfile`, see
[development.md](development.md)) also contains `mupdf-tools` and
`poppler-utils`. It is not published.
