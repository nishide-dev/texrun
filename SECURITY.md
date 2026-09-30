# Security Policy

## Supported versions

texrun is in early development and has no released versions yet. Security
fixes are made on the `main` branch.

## Reporting a vulnerability

Please **do not** open a public issue, pull request or discussion for a
security vulnerability.

Report it privately through GitHub's private vulnerability reporting:

1. Open the [Security tab](https://github.com/nishide-dev/texrun/security) of
   this repository.
2. Click **Report a vulnerability** and fill in the form.

Please include, if possible, the texrun commit, the OS, the TeX Live and
latexmk versions, and a minimal document or command that reproduces the issue.

We aim to acknowledge reports within 7 days. As this is a small project, fix
timelines depend on severity and maintainer availability; we will keep you
informed in the advisory.

報告は日本語でも英語でも構いません。

## Scope

texrun treats TeX documents as untrusted input, but the MVP runs TeX Live as a
local subprocess and is **not** a complete sandbox. What texrun does and does
not guarantee is described in [docs/security.md](docs/security.md) (Japanese).

- In scope: bypassing a boundary that docs/security.md §2 says texrun
  enforces, for example running commands through TeX or latexmk, reading or
  writing files outside the workspace through the routes listed there,
  inheriting host environment variables, or escaping the timeout, size limits
  or process cleanup.
- Out of scope: behaviour that docs/security.md §2 lists as not guaranteed in
  the MVP (for example font-related lookups, the PDF-object embedding
  primitives it mentions, the TeX Live tree being readable by name, and the
  lack of network or OS-level isolation), and vulnerabilities in TeX Live,
  latexmk, MuPDF or Poppler themselves; please report those upstream.

Please do not include working exploit inputs in public issues or pull
requests; send them through the private report instead.
