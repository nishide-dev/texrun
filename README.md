# texrun

> **Status: early development.** The repository currently contains only the
> Rust workspace skeleton. The features described below are the goals of the
> first milestone (MVP) and are **planned, not yet implemented**. Progress is
> tracked in [#13](https://github.com/nishide-dev/texrun/issues/13).

texrun is a frontend layer for compiling and inspecting TeX documents through a
safe, consistent interface, designed to be driven by AI agents and automated
environments as well as by people.

**日本語概要:** texrun は、AI エージェントや自動化環境から安全かつ一貫した
インターフェースで TeX 文書を compile / inspect するための Rust 製 CLI です。
TeX Live + latexmk を backend とし、compile の成否に加えて構造化 diagnostics・
PDF・ページ preview を返すことを MVP の目標としています（現在は初期開発段階で、
機能は未実装です）。

## Goal

Running `latexmk` directly gives you a PDF and a long, hard-to-parse log.
texrun wraps the TeX toolchain so that callers get:

- a predictable command-line interface with machine-readable (JSON) output,
- structured errors and warnings instead of raw log text,
- the PDF artifact and page previews,
- execution in an isolated temporary workspace with a timeout.

## MVP architecture

```text
Agent / User
    |
    v
texrun CLI
    |
    v
texrun core
    |
    v
engine interface
    |
    v
TeX Live + latexmk
    |
    +--> PDF
    +--> diagnostics
    +--> page previews
```

The engine interface keeps the core independent of a particular TeX
distribution or execution backend, so that other engines or stronger isolation
(e.g. a container worker) can be added later without breaking the core API.

## MVP scope

All items below are planned for the first milestone:

- TeX Live backend (via `latexmk`)
- local CLI
- isolated temporary workspace for each compile
- structured diagnostics (errors and warnings)
- PDF artifact
- page previews
- compile timeout

## Non-goals (first milestone)

- a full TeX parser implementation
- a web UI
- a hosted SaaS
- installing arbitrary packages during compile
- perfect in-process sandboxing
- support for multiple engines
- Windows support

## CLI example (planned)

> The `compile` command does not exist yet. The interface below is the intended
> design and may change; options, JSON shape and exit codes will be documented
> when [#6](https://github.com/nishide-dev/texrun/issues/6) is implemented.

```bash
# Human-readable output
texrun compile main.tex

# Machine-readable JSON on stdout (logs go to stderr)
texrun compile --json main.tex
```

## System requirements

- **OS:** Linux or macOS. Windows is not supported.
- **Rust:** 1.98.1. `rust-toolchain.toml` pins the version, so `rustup` installs
  and selects it automatically.
- **TeX Live + latexmk:** required to compile documents. For development, the
  Docker-based environment (see below) is recommended instead of installing
  TeX Live on the host.
- **Preview tool:** `mutool` (MuPDF) or `pdfinfo` + `pdftoppm` (Poppler), for
  page previews. MuPDF is used when both are installed; without either,
  compiling still works and the result says that previews were skipped.
  - Licensing: MuPDF is AGPL and Poppler is GPL. texrun only starts an
    installed binary as a separate process; it neither links nor ships them.
    To avoid MuPDF entirely, select the Poppler backend
    (`BackendChoice::Poppler`; a CLI option follows with #6) or do not install
    `mutool`.
  - The development Docker image below contains `mupdf-tools` and
    `poppler-utils`. If that image is ever distributed, the AGPL / GPL terms
    for distributing those packages apply to the image and must be checked
    separately.

## Local development

Run the quality gates before opening a pull request (the same checks as CI):

```bash
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

CI runs the tests with [cargo-nextest](https://nexte.st/) and also checks the
dependency policy in `deny.toml`. If you add or update dependencies, run:

```bash
cargo deny check
```

A Docker-based environment with the Rust toolchain, TeX Live, latexmk and a
preview tool is provided for running commands that need TeX, for example:

```bash
docker compose run --rm dev cargo test --workspace
```

See [docs/development.md](docs/development.md) for setup details.

## Security model

TeX can read files and, if enabled, run external commands, so texrun treats
documents as untrusted input. The MVP plans to disable shell escape, restrict
paths to the workspace, limit the environment passed to TeX and enforce a
timeout, but in-process execution is **not** a complete sandbox (for example,
parts of the host such as the TeX Live tree remain readable).

See [docs/security.md](docs/security.md) for the trust boundary, guarantees,
limitations and execution limits (Japanese), and [SECURITY.md](SECURITY.md) for
reporting vulnerabilities.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development flow, commit and
pull request conventions.

## License

texrun is licensed under the [MIT License](LICENSE).
