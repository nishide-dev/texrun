# Agent Skills

The repository ships an [Agent Skill](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview)
in [`skills/texrun/`](../skills/texrun/) that teaches an AI agent to use
texrun: compile with `--json`, fix the reported errors at their file and
line, recompile, and check the page previews. It tells the agent to use
`--backend container` for documents it does not trust.

| File | Loaded |
| --- | --- |
| [`SKILL.md`](../skills/texrun/SKILL.md) | When the agent decides the task is about compiling or fixing a LaTeX document |
| [`reference/json.md`](../skills/texrun/reference/json.md) | When it needs a field of the JSON result |
| [`reference/errors.md`](../skills/texrun/reference/errors.md) | When it needs the meaning of a diagnostic, error, note or preview notice kind |
| [`reference/setup.md`](../skills/texrun/reference/setup.md) | When texrun or the engine image is missing |
| [`scripts/texrun_summary.py`](../skills/texrun/scripts/texrun_summary.py) | Run, not read: turns the JSON into `path:line: kind: message` lines and preview paths (Python 3 standard library only) |

The skill runs the `texrun` command, so the agent needs a shell on a machine
where texrun is installed, with TeX Live on the host or Docker/Podman and the
engine image (see the [README](../README.md#installation)).

## Claude Code

### As a plugin (recommended)

The repository is also a Claude Code plugin marketplace
([`.claude-plugin/`](../.claude-plugin/)) with one plugin, `texrun`, that
contains the skill. In a Claude Code session:

```text
/plugin marketplace add nishide-dev/texrun
/plugin install texrun@texrun
```

or from a shell:

```bash
claude plugin marketplace add nishide-dev/texrun
claude plugin install texrun@texrun
```

Claude then uses the skill on its own when a task involves compiling or
fixing a LaTeX document; you can also invoke it as `/texrun:texrun`.
`claude plugin details texrun@texrun` shows that the skill is loaded.

The plugin follows the default branch: its version is the commit it was
installed from. To pick up changes, run `claude plugin marketplace update
texrun` and then `claude plugin update texrun@texrun` (or enable
auto-update for the marketplace under `/plugin`). The skill describes the
CLI of the same commit, so keep texrun itself up to date too.

To try a local checkout, add its path instead:
`claude plugin marketplace add ./texrun`, or load it for one session with
`claude --plugin-dir ./texrun`.

### As a personal or project skill

Without the plugin system, copy the skill directory:

```bash
git clone https://github.com/nishide-dev/texrun
# For you, in every project:
cp -R texrun/skills/texrun ~/.claude/skills/texrun
# Or for everyone working on a repository (commit it):
cp -R texrun/skills/texrun .claude/skills/texrun
```

It is then available as `/texrun`. Copies are not updated automatically.

## Other agents

The skill is a plain directory in the Agent Skills format (`SKILL.md` with
`name` and `description` frontmatter, plus reference files and a script).
An agent that supports this format and can run shell commands can load
`skills/texrun/`; see that agent's documentation for where skills go.

## Claude.ai and the Claude API

Both accept custom skills, but run them in Anthropic's code execution
environment, not on your machine. texrun and TeX Live are not installed
there, and on the Claude API that environment has no network access and
cannot install packages. The skill therefore cannot compile anything on
these surfaces; they are not supported. For reference, this is how a skill
is added there:

- **Claude.ai:** zip the directory and upload it under Settings >
  Features (Pro, Max, Team and Enterprise plans with code execution
  enabled):

  ```bash
  cd skills && zip -r texrun-skill.zip texrun
  ```

- **Claude API:** upload the files with `POST /v1/skills` (multipart
  `files[]`, each with its path such as `texrun/SKILL.md`) and reference the
  returned `skill_id` in `container.skills` of a Messages request together
  with the code execution tool. See
  [Using Agent Skills with the API](https://platform.claude.com/docs/en/build-with-claude/skills-guide).

To use texrun from an application built on the Claude API, run texrun in
your own tool implementation instead and return its JSON output to the
model.

## Keeping the skill in sync with the CLI

Agents follow the skill literally, so it must match the CLI. The unit tests
in [`apps/texrun/src/skill_docs.rs`](../apps/texrun/src/skill_docs.rs) (part
of `cargo test`) fail when:

- a `texrun ...` command in the skill is rejected by the CLI's argument
  parser, or an `--option` in it is not an option of `texrun compile`;
- a JSON field path in the skill (`preview.pages[].path`), in the tables or
  examples of `reference/json.md`, or read by the summary script, is not in
  the JSON document texrun prints;
- the kind tables of `reference/errors.md` do not list exactly the
  diagnostic, error, note and preview notice kinds of the implementation;
- the exit code table or the outcomes in `SKILL.md` do not match;
- the frontmatter breaks the Agent Skills rules (`name` at most 64
  lowercase letters, digits and hyphens; `description` at most 1024
  characters, no XML tags), the plugin manifests name another plugin, or a
  relative link does not resolve.

When you change an option, a JSON field or a kind, update `skills/texrun/`
in the same pull request. `claude plugin validate .` checks the plugin
manifests.
