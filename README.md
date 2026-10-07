# texrun

texrun compiles TeX documents through a safe, predictable command-line
interface. It is built as much for AI agents and automated pipelines as for
people: instead of a long latexmk log, you get structured diagnostics, the
PDF, and PNG previews of its pages, either as readable text or as a single
JSON document.

> [!NOTE]
> texrun is in early development (version 0.1.2). It works on Linux and
> macOS, but the CLI and the JSON output may still change.

## Why texrun

Running `latexmk` directly gives you a PDF and a log that is hard to parse,
and it runs TeX with full access to your files. texrun wraps TeX Live and
latexmk so that callers get:

- **Structured results.** Errors and warnings come with a kind, a file and a
  line, and every run reports its outcome and artifacts in a stable JSON
  schema.
- **The PDF and page previews.** The PDF, the log and PNG previews of the
  first pages are copied to an output directory.
- **Safe defaults.** Each compile runs in a fresh temporary workspace with
  shell escape disabled, a minimal environment, a timeout and resource
  limits. The container backend adds OS-level isolation with no network
  access.
- **Clear exit codes.** Success, a document error, a usage error, a runtime
  error and a timeout each have their own exit code.

## Installation

texrun runs on Linux and macOS; Windows is not supported. It needs Rust 1.98
or newer to build. Install it with Cargo:

```bash
cargo install --locked --git https://github.com/nishide-dev/texrun --tag v0.1.2 texrun
```

Then install the tools for the backend you plan to use:

- **Host backend (default):** TeX Live with `latexmk`. texrun does not
  install missing LaTeX packages; install them with TeX Live (for example,
  `tlmgr`).
- **Page previews on the host (optional):** `mutool` (MuPDF), or `pdfinfo`
  and `pdftoppm` (Poppler). Without either, texrun still compiles and
  reports that previews were skipped.
- **Container backend (optional):** Docker 20.10+ or Podman 4+, and the
  engine image that matches your texrun version. texrun never pulls images,
  so pull it once:

  ```bash
  docker pull ghcr.io/nishide-dev/texrun-engine:0.1.2
  ```

  The image includes TeX Live, latexmk and the preview tools, so nothing else
  is needed on the host.

## Quick start

Compile a document and read the result:

```console
$ texrun compile main.tex
main.tex:3: error: Undefined control sequence \foo
Failed to compile main.tex in 185ms (1 error, 0 warnings)
  log: texrun-out/main.log
```

After a successful compile, texrun prints the paths of the PDF and the
previews:

```console
$ texrun compile main.tex
Compiled main.tex in 222ms
  PDF: texrun-out/main.pdf
  preview: texrun-out/preview/page-001.png (1 of 1 pages, mupdf)
```

For tools and AI agents, `--json` prints exactly one JSON document on stdout,
even when the compile fails or texrun itself hits an error:

```bash
texrun compile --json main.tex
```

```json
{
  "schema_version": 1,
  "texrun_exit_code": 1,
  "outcome": "failed",
  "diagnostics": [
    {
      "severity": "error",
      "kind": "undefined_control_sequence",
      "message": "Undefined control sequence \\foo",
      "file": "main.tex",
      "line": 3
    }
  ],
  "artifacts": [
    { "kind": "log", "path": "main.log", "size_bytes": 2426 }
  ],
  "output_dir": "/home/me/paper/texrun-out"
}
```

(Some fields are omitted here.) Run `texrun compile --help` for all options.

## Backends

texrun can run TeX in two places, selected with `--backend`:

- **`host`** (default) runs the host's latexmk as your user. TeX's own
  restrictions and texrun's limits apply, but this is not a complete sandbox:
  TeX can still read parts of the host, such as the TeX Live tree, and
  network access is not blocked.
- **`container`** runs latexmk, and the page preview tools, in a hardened
  container built from the engine image (Docker or Podman). TeX sees only
  the workspace, has no network access and runs as a non-root user without
  capabilities, with a read-only root filesystem.

**Use `--backend container` for documents you do not trust:**

```bash
texrun compile --backend container main.tex
```

Docker (running as root, including Docker Desktop and OrbStack) and rootless
Podman are supported; rootless Docker is not. See
[docs/cli.md](docs/cli.md#engine-backends) for a detailed comparison and
[docs/engine-image.md](docs/engine-image.md) for pinning and verifying the
image.

## JSON output and exit codes

When reading the JSON document, check `error` first, then `outcome` (or just
`texrun_exit_code`):

- `outcome`: `succeeded`, `failed`, `timed_out` or `cancelled`.
- `diagnostics`: errors, warnings and info messages from TeX and BibTeX,
  each with a `severity`, a stable `kind`, a `message` and, when known, the
  `file` (relative to the project root) and `line`.
- `artifacts`: the PDF, the log and the previews, with paths relative to
  `output_dir`.
- `error`: present when texrun could not finish, with a `stage`, a stable
  `kind`, a `message` and often a `hint`.

| Exit code | Meaning |
| --- | --- |
| 0 | The document compiled and a PDF was produced |
| 1 | The document failed to compile (see the diagnostics) |
| 2 | Usage or input error |
| 3 | Runtime error (for example, latexmk or the engine image is missing) |
| 4 | The compile timed out |
| 128+N | Interrupted by signal N (130 for Ctrl-C) |

New fields and enum values may be added without changing `schema_version`.
The full schema, every error and note kind, and the details of each exit code
are in [docs/cli.md](docs/cli.md).

## Using texrun from AI agents

The repository includes an [Agent Skill](skills/texrun/SKILL.md) that
teaches an AI agent the compile-fix-preview loop: compile with `--json`,
fix the errors at the reported file and line, recompile, and look at the
page previews, using `--backend container` for untrusted documents. Install
texrun first, then add the skill to Claude Code as a plugin, at the
release tag of your texrun so that the skill matches its CLI (releases from
v0.1.1 contain the plugin):

```text
/plugin marketplace add nishide-dev/texrun@v<version>
/plugin install texrun@texrun
```

Or copy [`skills/texrun/`](skills/texrun/) to `~/.claude/skills/`, or to the
skills directory of another agent that supports the Agent Skills format. See
[docs/agent-skills.md](docs/agent-skills.md) for updating, texrun installed
from the default branch, the Claude Agent SDK, other agents, and why
Claude.ai and the Claude API are not supported.

## Security

texrun treats every document as untrusted input. It disables shell escape,
runs latexmk with its own configuration instead of any `latexmkrc` in the
project, restricts TeX's file access to the workspace where kpathsea allows
it, passes a minimal environment, and enforces a timeout and limits on
output size and CPU time. Memory and process limits for the whole compile
need the container backend, or a delegated cgroup on Linux (`--cgroup`).

With the default host backend, these measures do not make a complete
sandbox; use the container backend for untrusted documents (see
[Backends](#backends)).

- [docs/security.md](docs/security.md) describes the trust boundary and
  exactly what each backend guarantees (in Japanese).
- [SECURITY.md](SECURITY.md) explains how to report a vulnerability
  privately.

## Development

The repository is a Cargo workspace: the CLI is in `apps/texrun` and the
libraries are in `crates/`. A Docker-based development environment with TeX
Live, latexmk and the preview tools is provided.

Before opening a pull request, run the same checks as CI:

```bash
cargo check --workspace --all-targets --all-features --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow and conventions, and
[docs/development.md](docs/development.md) for the development environment
and the TeX Live integration tests (both in Japanese).

## License

texrun is licensed under the [MIT License](LICENSE).

The engine image contains no texrun code; it consists of unmodified Debian
packages and a few unmodified TeX Live packages under their own licenses,
including GPL and AGPL software, whose source is attached to each GitHub
release. See
[docs/engine-image.md](docs/engine-image.md#licenses) for details.
