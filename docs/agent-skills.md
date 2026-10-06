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
contains the skill.

The skill describes the CLI of the commit it comes from, so add the
marketplace **at the release tag of the texrun you installed** (the version
`texrun --version` prints; releases from v0.1.1 contain the plugin). In a
Claude Code session:

```text
/plugin marketplace add nishide-dev/texrun@v<version>
/plugin install texrun@texrun
```

or from a shell:

```bash
claude plugin marketplace add nishide-dev/texrun@v<version>
claude plugin install texrun@texrun
```

Claude then uses the skill on its own when a task involves compiling or
fixing a LaTeX document; you can also invoke it as `/texrun:texrun`.
`claude plugin details texrun@texrun` shows that the skill is loaded.

After upgrading texrun, move the skill to the new tag: `claude plugin
marketplace remove texrun` (this also uninstalls the plugin), then add the
marketplace at the new tag and install the plugin again.

If you installed texrun from the default branch (`cargo install` without
`--tag`), add the marketplace without a tag (`nishide-dev/texrun`) instead.
The plugin then follows the default branch: its version is the commit it
was installed from. To pick up changes, run `claude plugin marketplace
update texrun` and `claude plugin update texrun@texrun` (marketplaces other
than Anthropic's do not auto-update unless you enable it under `/plugin`),
and update texrun at the same time.

The skill also tells the agent that `texrun compile --help` and the actual
JSON win when they disagree with it.

To try a local checkout, add its path instead:
`claude plugin marketplace add ./texrun`, or load it for one session with
`claude --plugin-dir ./texrun`.

### As a personal or project skill

Without the plugin system, copy the skill directory:

```bash
git clone --branch v<version> https://github.com/nishide-dev/texrun
# For you, in every project:
cp -R texrun/skills/texrun ~/.claude/skills/texrun
# Or for everyone working on a repository (commit it):
cp -R texrun/skills/texrun .claude/skills/texrun
```

The skill is then available as `/texrun`. Copies are not updated automatically.

## Claude Agent SDK

The Agent SDK loads the plugin from a local directory, for example a
checkout at the tag of your texrun:

```python
from claude_agent_sdk import ClaudeAgentOptions

options = ClaudeAgentOptions(plugins=[{"type": "local", "path": "/path/to/texrun"}])
```

The skill then appears as `texrun:texrun` in the `skills` of the init
message. See
[Plugins in the SDK](https://code.claude.com/docs/en/agent-sdk/plugins).

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

To use texrun from your own application, build it on the
[Claude Agent SDK](#claude-agent-sdk), which runs on your machine and loads
this plugin as is, or, with the Claude API, run texrun in your own tool
implementation and return its JSON output to the model.

## Keeping the skill in sync with the CLI

Agents follow the skill literally, so it must match the CLI. The unit tests
in [`apps/texrun/src/skill_docs.rs`](../apps/texrun/src/skill_docs.rs) (part
of `cargo test`) fail when:

- a `texrun ...` command in the skill is rejected by the CLI's argument
  parser, or an `--option` in it is not an option of `texrun compile`;
- a JSON field path in the skill (`preview.pages[].path`), in the tables or
  examples of `reference/json.md`, or read by the summary script, is not in
  the JSON document texrun prints;
- a single `snake_case` word in a code span (`output_dir`,
  `texrun_exit_code`, `undefined_reference`, `succeeded`) is not a JSON
  field, a kind, a value of an enum or option, or one of a short list of
  other names in the test (`latexmk`, `mutool`, ...);
- the kind tables of `reference/errors.md` do not list exactly the
  diagnostic, error, note and preview notice kinds of the implementation
  (read from the enums and from the `kind` and `note` constants in
  `apps/texrun/src/report.rs`);
- the exit code table or the outcomes in `SKILL.md` do not match;
- the frontmatter breaks the Agent Skills rules (`name` at most 64
  lowercase letters, digits and hyphens; `description` at most 1024
  characters, no XML tags), the plugin manifests name another plugin, or a
  relative link does not resolve.

The tests compare the skill with the CLI of the same commit; they cannot
tell whether a user's texrun matches it. That is why the installation above
pins the marketplace to the texrun release.

When you change an option, a JSON field or a kind, update `skills/texrun/`
in the same pull request. `claude plugin validate .` checks the plugin
manifests.

## The repository root is the plugin

The marketplace entry's `source` is `"./"`, so the whole repository is the
plugin: installing it copies every tracked file (about 2 MB) into the
plugin cache, and Claude Code loads any plugin components it finds at the
root. Do not add these at the repository root unless they are meant for
the plugin's users: `commands/`, `agents/`, `hooks/hooks.json`,
`output-styles/`, `themes/`, `monitors/`, `bin/`, `settings.json`,
`.mcp.json`, `.lsp.json` (a top-level `bin/` also keeps claude.ai and
Cowork from installing the plugin). If the root needs one of them, move the
plugin into a subdirectory and change `source`.
