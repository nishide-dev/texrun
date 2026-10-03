# Security Policy

## Supported versions

texrun is in early development. Security fixes are made on the `main` branch
and included in the next release.

## Reporting a vulnerability

Please **do not** open a public issue, pull request or discussion for a
security vulnerability.

Report it privately through GitHub's private vulnerability reporting:

1. Open the [Security tab](https://github.com/nishide-dev/texrun/security) of
   this repository.
2. Click **Report a vulnerability** and fill in the form.

If possible, include the texrun version or commit, the OS, the backend
(`--backend host` or `--backend container`, and the container runtime), the
TeX Live and latexmk versions, and a minimal document or command that
reproduces the issue.

We aim to acknowledge reports within 7 days. As this is a small project, fix
timelines depend on severity and maintainer availability; we will keep you
informed in the advisory.

Reports may be written in English or Japanese.

## Scope

texrun treats TeX documents as untrusted input. It can run TeX in two ways,
and they enforce different boundaries:

- `--backend host` (the default) runs TeX Live on the host as your user. It
  is protected by TeX's own restrictions and texrun's limits, but it is
  **not** a complete sandbox.
- `--backend container` runs TeX and the page preview tools in a hardened
  container of the engine image. In addition to the host backend's
  guarantees, it confines them at the OS level: they see only the workspace
  and the image, have no network access and run without privileges.

Exactly what each backend guarantees is described in
[docs/security.md](docs/security.md) §2 (in Japanese).

### In scope

Bypassing a boundary that docs/security.md §2 says texrun enforces, for
example:

- with either backend: running commands through TeX or latexmk; reading or
  writing files outside the workspace through the routes listed there;
  inheriting host environment variables; or escaping the timeout, the size
  and resource limits, or process cleanup;
- with `--backend container`: reading host files other than the workspace,
  modifying the project's input files, reaching the network, gaining
  privileges inside the container, or exceeding the container's memory,
  process or CPU limits.

### Out of scope

- Limitations that docs/security.md §2 lists as not guaranteed. For
  `--backend host`, these include font-related lookups, the PDF-object
  embedding primitives it mentions, the TeX Live tree being readable by
  name, the lack of network blocking and of OS-level isolation, and
  process-tree limits without a delegated cgroup.
- Escaping a container through a vulnerability in the container runtime,
  the OCI runtime or the Linux kernel.
- Vulnerabilities in TeX Live, latexmk, MuPDF, Poppler, Docker or Podman
  themselves; please report those upstream.

Please do not include working exploit inputs in public issues or pull
requests; send them through the private report instead.
