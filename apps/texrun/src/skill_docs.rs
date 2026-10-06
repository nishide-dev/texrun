//! Checks that the Agent Skill in `skills/texrun/` matches the CLI.
//!
//! The skill is read by AI agents, which follow it literally, so a renamed
//! option, JSON field or kind would silently break them. These tests parse
//! the skill's Markdown and its summary script and compare them with the
//! clap definition, a fully populated JSON document and the kind enums:
//!
//! - frontmatter: `name` and `description` follow the Agent Skills rules,
//!   and the plugin manifests in `.claude-plugin/` name the same plugin;
//! - commands: every `texrun ...` snippet parses with clap, and every
//!   `--option` in a snippet is an option of `texrun compile`;
//! - JSON: every dotted field path (`preview.pages[].path`), every field in
//!   the tables of `reference/json.md`, the JSON examples and the fields the
//!   script reads with `.get("...")` exist in the JSON document;
//! - kinds: the kind tables of `reference/errors.md` list exactly the
//!   diagnostic, error, note and preview notice kinds (read from the enums
//!   and the `kind` / `note` constants);
//! - words: every single `snake_case` word in a code span is a JSON field,
//!   a kind, an enum or option value, or one of [`OTHER_WORDS`];
//! - exit codes: the table in `SKILL.md` lists exactly texrun's codes;
//! - links: every relative link points to an existing file.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use serde_json::{Value, json};
use texrun_core::schema::Versioned;
use texrun_core::{
    Artifact, CompileOutcome, CompileResult, DiagnosticKind, EngineErrorKind, Severity,
};
use texrun_preview::{NoticeKind, PreviewReport};
use texrun_workspace::WorkspaceErrorKind;

use crate::cli::Cli;
use crate::report::{
    Category, CompileReport, ErrorInfo, Note, ProjectInfo, Stage, WorkspaceInfo, exit, kind, note,
};

/// Options of other programs that appear in the skill (`cargo install`, the
/// summary script). Every other `--option` must be one of texrun's.
const FOREIGN_OPTIONS: &[&str] = &["--all", "--locked", "--git", "--tag"];

/// File extensions, so that `main.tex` is not taken for a JSON field path.
const FILE_EXTENSIONS: &[&str] = &[
    "aux", "bib", "blg", "bst", "cls", "json", "log", "md", "pdf", "png", "py", "sty", "tex",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn skill_dir() -> PathBuf {
    repo_root().join("skills/texrun")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every file of the skill with the given extension, relative path first.
fn skill_files(extension: &str) -> Vec<(String, String)> {
    fn walk(dir: &Path, base: &Path, extension: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, base, extension, out);
            } else if path.extension().is_some_and(|e| e == extension) {
                let name = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((name, read(&path)));
            }
        }
    }
    let mut out = Vec::new();
    let dir = skill_dir();
    walk(&dir, &dir, extension, &mut out);
    assert!(!out.is_empty(), "no .{extension} files in skills/texrun");
    out
}

/// The `---` frontmatter fields (single-line values) and the body.
fn frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let rest = text
        .strip_prefix("---\n")
        .expect("SKILL.md starts with --- frontmatter");
    let end = rest.find("\n---\n").expect("frontmatter ends with ---");
    let fields = rest[..end]
        .lines()
        .map(|line| {
            let (key, value) = line.split_once(':').expect("`key: value` frontmatter");
            (key.trim().to_owned(), value.trim().to_owned())
        })
        .collect();
    (fields, &rest[end + "\n---\n".len()..])
}

/// Code snippets of a Markdown file: inline code spans outside fenced
/// blocks, and the lines of fenced blocks (except JSON blocks).
fn code_snippets(markdown: &str) -> Vec<String> {
    let mut snippets = Vec::new();
    let mut fence: Option<String> = None;
    for line in markdown.lines() {
        if let Some(info) = line.trim_start().strip_prefix("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(info.trim().to_owned()),
            };
            continue;
        }
        match &fence {
            Some(info) if info == "json" || info == "jsonc" => {}
            Some(_) => snippets.push(line.trim().to_owned()),
            None => {
                let mut parts = line.split('`');
                parts.next();
                while let Some(code) = parts.next() {
                    snippets.push(code.to_owned());
                    parts.next();
                }
            }
        }
    }
    snippets
}

/// The fenced JSON code blocks of a Markdown file.
fn json_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(block) = &mut current {
            if trimmed.starts_with("```") {
                blocks.push(current.take().unwrap());
            } else {
                block.push_str(line);
                block.push('\n');
            }
        } else if trimmed == "```json" {
            current = Some(String::new());
        }
    }
    blocks
}

/// The first backticked cell of each table row in the section that starts
/// with `heading` (up to the next `## ` heading).
fn table_keys(markdown: &str, heading: &str) -> BTreeSet<String> {
    let start = markdown
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no `{heading}` section"));
    let section = &markdown[start + heading.len() + 2..];
    let section = section.find("\n## ").map_or(section, |end| &section[..end]);
    let keys: BTreeSet<String> = section
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .map(|rest| rest.split('`').next().unwrap().to_owned())
        .collect();
    assert!(!keys.is_empty(), "no table rows under `{heading}`");
    keys
}

/// `snake_case` names of the variants of `pub enum <name>` in Rust source.
fn enum_variants(source: &str, name: &str) -> BTreeSet<String> {
    let start = source
        .find(&format!("pub enum {name} {{"))
        .unwrap_or_else(|| panic!("no enum {name}"));
    let body = &source[start..];
    let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];
    body.lines()
        .map(str::trim)
        .filter(|line| line.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|line| {
            let ident: String = line
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            let mut snake = String::new();
            for (i, c) in ident.chars().enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    snake.push('_');
                }
                snake.push(c.to_ascii_lowercase());
            }
            snake
        })
        .collect()
}

fn source(path: &str) -> String {
    read(&repo_root().join(path))
}

/// A JSON document in which every field texrun can print is present.
fn full_document() -> Value {
    let result: CompileResult = serde_json::from_value(json!({
        "outcome": "failed",
        "engine": { "name": "texlive", "version": "latexmk 4.86" },
        "exit": { "code": 12, "signal": 9 },
        "elapsed_ms": 5,
        "diagnostics": [{
            "severity": "error", "kind": "undefined_control_sequence", "message": "m",
            "file": "main.tex", "line": 1, "raw_excerpt": "l.1"
        }],
        "artifacts": [{ "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 1 }],
        "resource_limits": { "rlimits": true, "cgroup": false, "notes": ["n"] }
    }))
    .unwrap();
    let preview: PreviewReport = serde_json::from_value(json!({
        "status": "partial",
        "backend": "mupdf",
        "format": "png",
        "pdf": { "page_count": 2, "pages": [
            { "page": 1, "width_pt": 595.0, "height_pt": 842.0, "rotation": 0 }
        ] },
        "pages": [{
            "kind": "preview", "path": "preview/page-001.png", "page": 1, "size_bytes": 1,
            "width_px": 1, "height_px": 1, "dpi": 144
        }],
        "notices": [{
            "severity": "warning", "kind": "render_failed", "message": "m", "page": 2, "detail": "d"
        }]
    }))
    .unwrap();
    let not_copied: Vec<Artifact> =
        serde_json::from_value(json!([{ "kind": "pdf", "path": "main.pdf" }])).unwrap();
    let mut workspace = WorkspaceInfo::default();
    workspace.excluded.push(crate::report::ExcludedInfo {
        path: "latexmkrc".to_owned(),
        reason: "tool_config",
    });
    workspace.kept_path = Some("/tmp/ws".to_owned());
    let report = CompileReport {
        texrun_exit_code: Some(3),
        result: Some(result),
        output_dir: Some("/out".to_owned()),
        artifacts_not_copied: not_copied,
        notes: vec![Note {
            severity: Severity::Info,
            kind: note::PARENT_DIRECTORY_INPUT,
            message: "m".to_owned(),
            printed: false,
        }],
        preview: Some(preview),
        project: Some(ProjectInfo {
            root: "/p".to_owned(),
            entrypoint: "main.tex".to_owned(),
        }),
        workspace: Some(workspace),
        error: Some(
            ErrorInfo::new(Stage::Collect, kind::IO, Category::Runtime, "m").with_hint("h"),
        ),
    };
    serde_json::to_value(Versioned::new(report)).unwrap()
}

/// Every field path of a JSON value: `a`, `a.b`, `a[]`, `a[].b`.
fn field_paths(value: &Value) -> BTreeSet<String> {
    fn walk(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    out.insert(path.clone());
                    walk(child, &path, out);
                }
            }
            Value::Array(items) => {
                let path = format!("{prefix}[]");
                out.insert(path.clone());
                for item in items {
                    walk(item, &path, out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(value, "", &mut out);
    out
}

/// Whether `code` looks like a dotted JSON field path (`error.kind`,
/// `preview.pages[].path`) rather than a file name.
fn is_field_path(code: &str) -> bool {
    let segments: Vec<&str> = code.split('.').collect();
    segments.len() > 1
        && segments.iter().all(|s| {
            let name = s.strip_suffix("[]").unwrap_or(s);
            !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        })
        && !FILE_EXTENSIONS.contains(segments.last().unwrap())
}

#[test]
fn frontmatter_follows_the_agent_skills_rules() {
    let text = read(&skill_dir().join("SKILL.md"));
    let (fields, body) = frontmatter(&text);
    let get = |key: &str| {
        fields.iter().find(|(k, _)| k == key).map_or_else(
            || panic!("no `{key}` in the frontmatter"),
            |(_, v)| v.as_str(),
        )
    };
    let name = get("name");
    assert!(name.len() <= 64, "name is longer than 64 characters");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "name may only contain lowercase letters, digits and hyphens"
    );
    assert!(!name.contains("anthropic") && !name.contains("claude"));
    assert_eq!(name, "texrun", "the name matches the directory");
    let description = get("description");
    assert!(!description.is_empty());
    assert!(
        description.chars().count() <= 1024,
        "description has {} characters (at most 1024)",
        description.chars().count()
    );
    assert!(!description.contains('<') && !description.contains('>'));
    assert!(
        body.lines().count() < 500,
        "keep the SKILL.md body under 500 lines"
    );

    for manifest in ["marketplace.json", "plugin.json"] {
        let path = repo_root().join(".claude-plugin").join(manifest);
        let value: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(value["name"], "texrun", "{manifest}");
    }
    let marketplace: Value =
        serde_json::from_str(&read(&repo_root().join(".claude-plugin/marketplace.json"))).unwrap();
    assert_eq!(marketplace["plugins"][0]["name"], "texrun");
    assert_eq!(marketplace["plugins"][0]["source"], "./");
}

#[test]
fn commands_and_options_exist() {
    let command = Cli::command();
    let compile = command
        .find_subcommand("compile")
        .expect("compile subcommand");
    let options: BTreeSet<String> = compile
        .get_arguments()
        .filter_map(|a| a.get_long())
        .map(|long| format!("--{long}"))
        .chain(["--help".to_owned(), "--version".to_owned()])
        .collect();
    // Options named without their value in prose (`--pages`).
    let takes_value: BTreeSet<String> = compile
        .get_arguments()
        .filter(|a| a.get_action().takes_values())
        .filter_map(|a| a.get_long())
        .map(|long| format!("--{long}"))
        .collect();

    let mut texrun_commands = 0;
    for (file, text) in skill_files("md") {
        for snippet in code_snippets(&text) {
            for word in snippet.split_whitespace() {
                let option = word.split('=').next().unwrap();
                if option.starts_with("--")
                    && option.len() > 2
                    && !FOREIGN_OPTIONS.contains(&option)
                {
                    assert!(
                        options.contains(option),
                        "{file}: `{option}` is not an option of texrun compile (in `{snippet}`)"
                    );
                }
            }
            // A whole command, or options with concrete values: clap must
            // accept them as written.
            let args: Option<Vec<&str>> = if snippet.starts_with("texrun ") {
                let command = snippet.split(['|', '>']).next().unwrap();
                Some(command.split_whitespace().collect())
            } else if snippet.starts_with("--")
                && !snippet.contains('<')
                && !FOREIGN_OPTIONS.contains(&snippet.split_whitespace().next().unwrap())
                && !takes_value.contains(snippet.as_str())
            {
                let mut args = vec!["texrun", "compile"];
                args.extend(snippet.split_whitespace());
                args.push("main.tex");
                Some(args)
            } else {
                None
            };
            if let Some(args) = args {
                texrun_commands += 1;
                if let Err(e) = Cli::try_parse_from(&args) {
                    use clap::error::ErrorKind as K;
                    assert!(
                        // `texrun compile --json` in prose has no entrypoint.
                        matches!(
                            e.kind(),
                            K::DisplayHelp | K::DisplayVersion | K::MissingRequiredArgument
                        ),
                        "{file}: `{snippet}` is rejected by texrun: {e}"
                    );
                }
            }
        }
    }
    assert!(
        texrun_commands >= 5,
        "found only {texrun_commands} commands"
    );
}

#[test]
fn json_fields_exist() {
    let paths: BTreeSet<String> = field_paths(&full_document())
        .into_iter()
        .flat_map(|p| {
            let bare = p.strip_suffix("[]").map(str::to_owned);
            [Some(p), bare].into_iter().flatten()
        })
        .collect();
    let leaves: BTreeSet<&str> = paths
        .iter()
        .map(|p| p.rsplit('.').next().unwrap().trim_end_matches("[]"))
        .collect();

    for (file, text) in skill_files("md") {
        for code in code_snippets(&text) {
            if is_field_path(&code) {
                assert!(paths.contains(&code), "{file}: no JSON field `{code}`");
            }
        }
        for block in json_blocks(&text) {
            let value: Value = serde_json::from_str(&block)
                .unwrap_or_else(|e| panic!("{file}: invalid JSON example: {e}"));
            for path in field_paths(&value) {
                assert!(
                    paths.contains(&path),
                    "{file}: the JSON example has `{path}`, which texrun does not print"
                );
            }
        }
    }

    let json_md = read(&skill_dir().join("reference/json.md"));
    for heading in [
        "## Top level",
        "## Diagnostics",
        "## Artifacts and previews",
        "## Project and workspace",
    ] {
        for field in table_keys(&json_md, heading) {
            assert!(
                paths.contains(&field),
                "reference/json.md: no JSON field `{field}`"
            );
        }
    }

    for (file, script) in skill_files("py") {
        for quote in ['"', '\''] {
            let marker = format!(".get({quote}");
            for (i, _) in script.match_indices(&marker) {
                let rest = &script[i + marker.len()..];
                let field = &rest[..rest.find(quote).unwrap()];
                assert!(
                    leaves.contains(field),
                    "{file}: reads `{field}`, which texrun does not print"
                );
            }
        }
    }
}

/// The string values of the `pub const NAME: &str = "value";` items in
/// `pub mod <module> { ... }` of Rust source.
fn module_consts(source: &str, module: &str) -> BTreeSet<String> {
    let start = source
        .find(&format!("pub mod {module} {{"))
        .unwrap_or_else(|| panic!("no mod {module}"));
    let body = &source[start..];
    let body = &body[..body.find("\n}").unwrap()];
    let values: BTreeSet<String> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub const "))
        .filter_map(|rest| rest.split('"').nth(1))
        .map(str::to_owned)
        .collect();
    assert!(!values.is_empty(), "no constants in mod {module}");
    values
}

/// `diagnostics[].kind` values.
fn diagnostic_kinds() -> BTreeSet<String> {
    let kinds = enum_variants(
        &source("crates/texrun-core/src/diagnostic.rs"),
        "DiagnosticKind",
    );
    for name in &kinds {
        let kind: DiagnosticKind = serde_json::from_value(json!(name)).unwrap();
        assert!(name == "other" || kind != DiagnosticKind::Other, "{name}");
    }
    kinds
}

/// `error.kind` values: the CLI's own, the engine's and the workspace's.
fn error_kinds() -> BTreeSet<String> {
    let engine = enum_variants(
        &source("crates/texrun-core/src/engine.rs"),
        "EngineErrorKind",
    );
    let workspace = enum_variants(
        &source("crates/texrun-workspace/src/error.rs"),
        "WorkspaceErrorKind",
    );
    for name in &engine {
        serde_json::from_value::<EngineErrorKind>(json!(name)).unwrap();
    }
    for name in &workspace {
        serde_json::from_value::<WorkspaceErrorKind>(json!(name)).unwrap();
    }
    let cli = module_consts(&source("apps/texrun/src/report.rs"), "kind");
    // The parser reads the source; make sure it saw the real constants.
    assert!(cli.contains(kind::USAGE) && cli.contains(kind::UNSUPPORTED));
    let mut kinds = cli;
    kinds.extend(engine);
    kinds.extend(workspace);
    kinds
}

/// `notes[].kind` values.
fn note_kinds() -> BTreeSet<String> {
    let kinds = module_consts(&source("apps/texrun/src/report.rs"), "note");
    assert!(kinds.contains(note::PARENT_DIRECTORY_INPUT));
    kinds
}

/// `preview.notices[].kind` values.
fn notice_kinds() -> BTreeSet<String> {
    let kinds = enum_variants(&source("crates/texrun-preview/src/report.rs"), "NoticeKind");
    for name in &kinds {
        let kind: NoticeKind = serde_json::from_value(json!(name)).unwrap();
        assert!(name == "other" || kind != NoticeKind::Other, "{name}");
    }
    kinds
}

/// Every value of a `snake_case` enum or option that can appear in the JSON
/// document or on the command line.
fn enum_values() -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    for (path, name) in [
        ("crates/texrun-core/src/diagnostic.rs", "Severity"),
        ("crates/texrun-core/src/result.rs", "CompileOutcome"),
        ("crates/texrun-core/src/artifact.rs", "ArtifactKind"),
        ("crates/texrun-preview/src/report.rs", "PreviewStatus"),
        ("crates/texrun-preview/src/report.rs", "BackendKind"),
        ("crates/texrun-preview/src/report.rs", "ImageFormat"),
        ("apps/texrun/src/report.rs", "Stage"),
        ("apps/texrun/src/report.rs", "Category"),
    ] {
        values.extend(enum_variants(&source(path), name));
    }
    // `workspace.excluded[].reason`.
    let report = source("apps/texrun/src/report.rs");
    let reasons = &report[report.find("pub fn exclusion_reason").unwrap()..];
    let reasons = &reasons[..reasons.find("\n}").unwrap()];
    values.extend(
        reasons
            .lines()
            .filter_map(|line| line.split("=> \"").nth(1))
            .map(|rest| rest.trim_end_matches(['"', ',']).to_owned()),
    );
    // Values of the options (`host`, `container`, `docker`, ...).
    let command = Cli::command();
    let compile = command.find_subcommand("compile").unwrap();
    for arg in compile.get_arguments() {
        for value in arg.get_possible_values() {
            values.insert(value.get_name().to_owned());
        }
    }
    values
}

#[test]
fn kind_tables_list_every_kind() {
    let errors_md = read(&skill_dir().join("reference/errors.md"));
    assert_eq!(
        table_keys(&errors_md, "## Diagnostic kinds"),
        diagnostic_kinds()
    );
    assert_eq!(table_keys(&errors_md, "## Error kinds"), error_kinds());
    assert_eq!(table_keys(&errors_md, "## Note kinds"), note_kinds());
    assert_eq!(
        table_keys(&errors_md, "## Preview notice kinds"),
        notice_kinds()
    );
}

/// Single `snake_case` words in code spans that are not texrun's: programs,
/// files and TeX names the skill mentions.
const OTHER_WORDS: &[&str] = &[
    "latexmk",
    "latexmkrc",
    "minted",
    "mutool",
    "pdfinfo",
    "pdftoppm",
    "texlive",
    "texrun",
];

#[test]
fn single_word_names_exist() {
    let mut known: BTreeSet<String> = field_paths(&full_document())
        .iter()
        .map(|p| {
            p.rsplit('.')
                .next()
                .unwrap()
                .trim_end_matches("[]")
                .to_owned()
        })
        .collect();
    known.extend(diagnostic_kinds());
    known.extend(error_kinds());
    known.extend(note_kinds());
    known.extend(notice_kinds());
    known.extend(enum_values());
    known.extend(OTHER_WORDS.iter().map(|w| (*w).to_owned()));

    for (file, text) in skill_files("md") {
        for code in code_snippets(&text) {
            let is_word = code.starts_with(|c: char| c.is_ascii_lowercase())
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if is_word {
                assert!(
                    known.contains(&code),
                    "{file}: `{code}` is not a JSON field, kind or value of texrun \
                     (add it to OTHER_WORDS if it names something else)"
                );
            }
        }
    }
}

#[test]
fn outcomes_and_exit_codes_match() {
    let skill = read(&skill_dir().join("SKILL.md"));
    for outcome in enum_variants(
        &source("crates/texrun-core/src/result.rs"),
        "CompileOutcome",
    ) {
        serde_json::from_value::<CompileOutcome>(json!(outcome)).unwrap();
        assert!(
            skill.contains(&format!("- `{outcome}`:")),
            "SKILL.md does not say what to do on outcome `{outcome}`"
        );
    }

    let start = skill
        .find("\n## Exit codes\n")
        .expect("an Exit codes section");
    let codes: BTreeSet<u8> = skill[start..]
        .lines()
        .skip(2)
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|rest| rest.split(' ').next()?.parse().ok())
        .collect();
    let expected: BTreeSet<u8> = [
        exit::SUCCESS,
        exit::COMPILE_FAILED,
        exit::USAGE,
        exit::RUNTIME,
        exit::TIMED_OUT,
        exit::CANCELLED,
    ]
    .into();
    assert_eq!(codes, expected);
}

#[test]
fn relative_links_resolve() {
    for (file, text) in skill_files("md") {
        let dir = skill_dir().join(&file).parent().unwrap().to_owned();
        for (i, _) in text.match_indices("](") {
            let target = &text[i + 2..];
            let target = &target[..target.find(')').unwrap()];
            if target.starts_with("http") || target.starts_with('#') {
                continue;
            }
            let target = target.split('#').next().unwrap();
            assert!(
                dir.join(target).exists(),
                "{file}: link to `{target}` does not resolve"
            );
        }
    }
}
