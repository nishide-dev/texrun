//! Workspace behaviour on a real filesystem. No TeX installation needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use texrun_core::testing::{FakeBehavior, FakeEngine};
use texrun_core::{
    Artifact, ArtifactKind, CompileOptions, CompileOutcome, CompileResult, EngineError,
    EngineErrorKind, EngineInfo, TypesetEngine, WorkspacePath,
};
use texrun_workspace::{
    ExclusionReason, Limit, OverwritePolicy, ProjectInput, Workspace, WorkspaceConfig,
    WorkspaceError, WorkspaceLimits,
};

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

/// A project directory plus a private parent for workspaces, so tests can
/// check that nothing is left behind.
struct Fixture {
    project: TempDir,
    temp_parent: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Self {
            project: tempfile::tempdir().unwrap(),
            temp_parent: tempfile::tempdir().unwrap(),
        };
        write(f.root(), "main.tex", "\\documentclass{article}");
        f
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn config(&self) -> WorkspaceConfig {
        WorkspaceConfig::default().with_temp_parent(self.temp_parent.path())
    }

    fn input(&self, entry: &str) -> ProjectInput {
        ProjectInput::new(self.root(), entry).unwrap()
    }

    fn create(&self, entry: &str) -> Result<Workspace, WorkspaceError> {
        Workspace::create(
            &ProjectInput::new(self.root(), entry)?,
            CompileOptions::default(),
            &self.config(),
        )
    }

    fn leftover_workspaces(&self) -> usize {
        fs::read_dir(self.temp_parent.path()).unwrap().count()
    }
}

fn wp(s: &str) -> WorkspacePath {
    WorkspacePath::new(s).unwrap()
}

#[cfg(unix)]
fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

// --- entrypoints and paths ------------------------------------------------

#[test]
fn nested_entrypoint_and_files_are_materialized() {
    let f = Fixture::new();
    write(f.root(), "paper/src/main.tex", "\\input{chapters/intro}");
    write(f.root(), "paper/src/chapters/intro.tex", "intro");
    write(f.root(), "paper/figs/plot.pdf", "%PDF");

    let ws = f.create("./paper//src/main.tex").unwrap();
    assert_eq!(ws.request().entrypoint.as_str(), "paper/src/main.tex");
    assert!(ws.path().is_absolute());
    assert!(!ws.path().starts_with(f.root()));
    assert_eq!(read(ws.path(), "paper/src/chapters/intro.tex"), "intro");
    assert_eq!(read(ws.path(), "paper/figs/plot.pdf"), "%PDF");
    assert_eq!(read(ws.path(), "main.tex"), "\\documentclass{article}");
    assert!(ws.output_dir().is_dir());
    assert_eq!(ws.report().bytes, 54);
    // main.tex, paper, paper/figs, paper/figs/plot.pdf, paper/src,
    // paper/src/chapters, paper/src/chapters/intro.tex, paper/src/main.tex
    assert_eq!(ws.report().entries, 8);

    let ctx = ws.context();
    assert_eq!(ctx.workspace.path(), ws.path());
    assert_eq!(
        ctx.workspace.resolve(&ws.request().entrypoint),
        ws.path().join("paper/src/main.tex")
    );
}

#[test]
fn host_entrypoint_defaults_root_to_its_directory() {
    let f = Fixture::new();
    write(f.root(), "paper/main.tex", "paper");
    write(f.root(), "paper/sec.tex", "sec");

    let input = ProjectInput::from_host_entrypoint(&f.root().join("paper/main.tex"), None).unwrap();
    assert_eq!(input.entrypoint().as_str(), "main.tex");
    assert_eq!(
        input.root(),
        fs::canonicalize(f.root().join("paper")).unwrap()
    );
    let ws = Workspace::create(&input, CompileOptions::default(), &f.config()).unwrap();
    assert_eq!(read(ws.path(), "sec.tex"), "sec");
    // The parent project is not part of the input.
    assert!(!ws.path().join("paper").exists());

    let with_root =
        ProjectInput::from_host_entrypoint(&f.root().join("paper/main.tex"), Some(f.root()))
            .unwrap();
    assert_eq!(with_root.entrypoint().as_str(), "paper/main.tex");
}

#[test]
fn parent_traversal_is_rejected() {
    let f = Fixture::new();
    let outer = f.root().parent().unwrap().join("secret.tex");
    for entry in ["../secret.tex", "a/../../secret.tex", ".."] {
        assert!(
            matches!(
                ProjectInput::new(f.root(), entry),
                Err(WorkspaceError::InvalidEntrypoint { .. })
            ),
            "{entry}"
        );
    }
    // Host form: an entrypoint outside an explicit root.
    let sub = f.root().join("sub");
    fs::create_dir(&sub).unwrap();
    assert!(matches!(
        ProjectInput::from_host_entrypoint(&sub.join("../main.tex"), Some(&sub)),
        Err(WorkspaceError::EntrypointOutsideRoot { .. })
    ));
    assert!(!outer.exists());
}

#[test]
fn absolute_entrypoint_is_rejected() {
    let f = Fixture::new();
    let abs = f.root().join("main.tex");
    for entry in [abs.to_str().unwrap(), "/etc/passwd", "C:/x.tex"] {
        assert!(
            matches!(
                ProjectInput::new(f.root(), entry),
                Err(WorkspaceError::InvalidEntrypoint { .. })
            ),
            "{entry}"
        );
    }
}

#[test]
fn missing_entrypoint_is_an_error() {
    let f = Fixture::new();
    assert!(matches!(
        ProjectInput::new(f.root(), "nope.tex"),
        Err(WorkspaceError::EntrypointNotFound(_))
    ));
    assert!(matches!(
        ProjectInput::from_host_entrypoint(&f.root().join("nope/main.tex"), None),
        Err(WorkspaceError::EntrypointNotFound(_))
    ));
    fs::create_dir(f.root().join("dir.tex")).unwrap();
    assert!(matches!(
        ProjectInput::new(f.root(), "dir.tex"),
        Err(WorkspaceError::EntrypointNotFile(_))
    ));
    assert!(matches!(
        ProjectInput::new(f.root().join("missing-root"), "main.tex"),
        Err(WorkspaceError::RootNotDirectory(_))
    ));
}

#[test]
fn entrypoint_outside_root_is_an_error() {
    let f = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    write(other.path(), "main.tex", "elsewhere");
    assert!(matches!(
        ProjectInput::from_host_entrypoint(&other.path().join("main.tex"), Some(f.root())),
        Err(WorkspaceError::EntrypointOutsideRoot { .. })
    ));
}

#[cfg(unix)]
#[test]
fn entrypoint_symlink_escaping_root_is_an_error() {
    let f = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    write(other.path(), "evil.tex", "evil");
    symlink(other.path().join("evil.tex"), f.root().join("link.tex"));
    assert!(matches!(
        ProjectInput::new(f.root(), "link.tex"),
        Err(WorkspaceError::EntrypointOutsideRoot { .. })
    ));
}

#[test]
fn entrypoint_in_excluded_location_is_rejected() {
    let f = Fixture::new();
    write(f.root(), ".git/main.tex", "x");
    write(f.root(), "build/main.tex", "x");

    assert!(matches!(
        f.create(".git/main.tex"),
        Err(WorkspaceError::EntrypointExcluded(_))
    ));
    // Entrypoint inside the configured output directory.
    let err = Workspace::create(
        &f.input("build/main.tex"),
        CompileOptions::default().with_output_dir(wp("build")),
        &f.config(),
    )
    .unwrap_err();
    let WorkspaceError::InvalidRequest(inner) = err else {
        panic!("unexpected {err:?}");
    };
    assert_eq!(inner.kind(), EngineErrorKind::InvalidRequest);
    assert_eq!(f.leftover_workspaces(), 0);
}

// --- exclusions -----------------------------------------------------------

#[test]
fn latexmkrc_and_vcs_and_output_dirs_are_not_copied() {
    let f = Fixture::new();
    write(f.root(), "latexmkrc", "system('rm -rf ~');");
    write(f.root(), ".latexmkrc", "evil");
    write(f.root(), "sub/LATEXMKRC", "evil");
    write(f.root(), ".git/config", "[core]");
    write(f.root(), ".texrun/out/stale.pdf", "stale");
    write(f.root(), "target/debug/x", "x");
    write(f.root(), "sub/keep.tex", "keep");

    let ws = f.create("main.tex").unwrap();
    for gone in [
        "latexmkrc",
        ".latexmkrc",
        "sub/LATEXMKRC",
        ".git",
        ".texrun/out/stale.pdf",
        "target",
    ] {
        assert!(
            fs::symlink_metadata(ws.path().join(gone)).is_err(),
            "{gone} should not be copied"
        );
    }
    assert_eq!(read(ws.path(), "sub/keep.tex"), "keep");
    // The output directory exists but starts empty.
    assert_eq!(fs::read_dir(ws.output_dir()).unwrap().count(), 0);

    let rc: Vec<_> = ws
        .report()
        .excluded_with(ExclusionReason::LatexmkRc)
        .map(|e| e.path.clone())
        .collect();
    assert_eq!(
        rc,
        [".latexmkrc", "latexmkrc", "sub/LATEXMKRC"].map(PathBuf::from)
    );
    let names: Vec<_> = ws
        .report()
        .excluded_with(ExclusionReason::ExcludedName)
        .map(|e| e.path.clone())
        .collect();
    assert_eq!(names, [".git", ".texrun", "target"].map(PathBuf::from));
}

#[test]
fn custom_output_dir_is_excluded_and_other_names_configurable() {
    let f = Fixture::new();
    write(f.root(), "build/out/old.pdf", "old");
    write(f.root(), "build/notes.tex", "notes");
    write(f.root(), "node_modules/x.js", "x");
    write(f.root(), ".git/config", "kept now");

    let config = f.config().with_excluded_names(["node_modules"]);
    let ws = Workspace::create(
        &f.input("main.tex"),
        CompileOptions::default().with_output_dir(wp("build/out")),
        &config,
    )
    .unwrap();
    assert_eq!(read(ws.path(), "build/notes.tex"), "notes");
    assert!(!ws.path().join("build/out/old.pdf").exists());
    assert!(!ws.path().join("node_modules").exists());
    assert_eq!(read(ws.path(), ".git/config"), "kept now");
    assert_eq!(
        ws.report()
            .excluded_with(ExclusionReason::OutputDirectory)
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        [PathBuf::from("build/out")]
    );
}

#[cfg(unix)]
#[test]
fn special_files_are_skipped() {
    let f = Fixture::new();
    let _listener = std::os::unix::net::UnixListener::bind(f.root().join("s.sock")).unwrap();
    let ws = f.create("main.tex").unwrap();
    assert!(!ws.path().join("s.sock").exists());
    assert_eq!(
        ws.report()
            .excluded_with(ExclusionReason::SpecialFile)
            .count(),
        1
    );
}

// --- symlinks -------------------------------------------------------------

#[cfg(unix)]
#[test]
fn symlinks_outside_root_are_rejected() {
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "secret.txt", "secret");

    let cases: [(&str, PathBuf); 4] = [
        ("file.tex", outside.path().join("secret.txt")),
        ("dir", outside.path().to_path_buf()),
        (
            "rel.tex",
            PathBuf::from("../../../../../../../../etc/passwd"),
        ),
        // Dangling, but lexically outside.
        ("dangling.tex", outside.path().join("missing.txt")),
    ];
    for (name, target) in cases {
        let f = Fixture::new();
        symlink(&target, f.root().join(name));
        let err = f.create("main.tex").unwrap_err();
        assert!(
            matches!(&err, WorkspaceError::SymlinkOutsideRoot { link, .. } if link == Path::new(name)),
            "{name}: {err:?}"
        );
        assert_eq!(f.leftover_workspaces(), 0, "{name}: cleaned up on error");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_inside_root_are_recreated_relative_to_the_workspace() {
    let f = Fixture::new();
    write(f.root(), "shared/macros.tex", "macros");
    fs::create_dir(f.root().join("sub")).unwrap();
    // Absolute and relative links, to a file and to a directory.
    symlink(
        f.root().join("shared/macros.tex"),
        f.root().join("sub/abs.tex"),
    );
    symlink("../shared", f.root().join("sub/dir"));
    // Links into excluded locations and dangling links are left out.
    write(f.root(), ".git/config", "git");
    symlink(".git/config", f.root().join("git.tex"));
    symlink("nowhere.tex", f.root().join("dangling.tex"));

    let ws = f.create("main.tex").unwrap();
    let abs = ws.path().join("sub/abs.tex");
    assert!(fs::symlink_metadata(&abs).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::read_link(&abs).unwrap(),
        Path::new("../shared/macros.tex")
    );
    assert_eq!(read(ws.path(), "sub/abs.tex"), "macros");
    assert_eq!(read(ws.path(), "sub/dir/macros.tex"), "macros");
    for link in ["sub/abs.tex", "sub/dir"] {
        let resolved = fs::canonicalize(ws.path().join(link)).unwrap();
        assert!(
            resolved.starts_with(ws.path()),
            "{link} stays in the workspace"
        );
    }
    assert!(fs::symlink_metadata(ws.path().join("git.tex")).is_err());
    assert!(fs::symlink_metadata(ws.path().join("dangling.tex")).is_err());
    let report = ws.report();
    assert_eq!(
        report
            .excluded_with(ExclusionReason::SymlinkToExcluded)
            .count(),
        1
    );
    assert_eq!(
        report
            .excluded_with(ExclusionReason::UnresolvableSymlink)
            .count(),
        1
    );

    // Removing the workspace must not follow links into the project.
    drop(ws);
    assert_eq!(read(f.root(), "shared/macros.tex"), "macros");
}

// --- limits ---------------------------------------------------------------

#[test]
fn size_limits_are_enforced() {
    let f = Fixture::new();
    write(f.root(), "big.bin", &"x".repeat(1000));
    write(f.root(), "a/b/c/d.tex", "deep");

    let run = |limits: WorkspaceLimits| {
        Workspace::create(
            &f.input("main.tex"),
            CompileOptions::default(),
            &f.config().with_limits(limits),
        )
    };
    let bytes = run(WorkspaceLimits::default().with_max_total_bytes(500)).unwrap_err();
    assert!(
        matches!(
            bytes,
            WorkspaceError::LimitExceeded {
                limit: Limit::TotalBytes,
                max: 500
            }
        ),
        "{bytes:?}"
    );
    let entries = run(WorkspaceLimits::default().with_max_entries(3)).unwrap_err();
    assert!(matches!(
        entries,
        WorkspaceError::LimitExceeded {
            limit: Limit::Entries,
            max: 3
        }
    ));
    let depth = run(WorkspaceLimits::default().with_max_depth(3)).unwrap_err();
    assert!(matches!(
        depth,
        WorkspaceError::LimitExceeded {
            limit: Limit::Depth,
            max: 3
        }
    ));
    assert_eq!(f.leftover_workspaces(), 0, "failed workspaces are removed");

    // Exactly at the limits is fine; excluded files do not count.
    write(f.root(), ".git/huge", &"y".repeat(10_000));
    let exact = WorkspaceLimits::default()
        .with_max_total_bytes(1000 + 23 + 4)
        .with_max_entries(7)
        .with_max_depth(4);
    let ws = run(exact).unwrap();
    assert_eq!(ws.report().bytes, 1027);
}

#[test]
fn default_limits() {
    let limits = WorkspaceLimits::default();
    assert_eq!(limits.max_total_bytes, 256 * 1024 * 1024);
    assert_eq!(limits.max_entries, 10_000);
    assert_eq!(limits.max_depth, 32);
}

// --- concurrency and cleanup ----------------------------------------------

#[test]
fn concurrent_workspaces_do_not_collide() {
    let f = Fixture::new();
    let input = f.input("main.tex");
    let config = f.config();

    let paths: Vec<PathBuf> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (input, config) = (&input, &config);
                s.spawn(move || {
                    let mut ws =
                        Workspace::create(input, CompileOptions::default(), config).unwrap();
                    let out = ws.output_dir().join("main.pdf");
                    fs::write(&out, format!("pdf {i}")).unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                    assert_eq!(fs::read_to_string(&out).unwrap(), format!("pdf {i}"));
                    ws.set_keep(true);
                    ws.path().to_path_buf()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 8);
    assert_eq!(f.leftover_workspaces(), 8);
    assert!(!f.root().join(".texrun").exists(), "project is untouched");
}

#[test]
fn workspace_is_removed_on_drop_unless_kept() {
    let f = Fixture::new();
    let ws = f.create("main.tex").unwrap();
    let path = ws.path().to_path_buf();
    assert!(path.is_dir());
    drop(ws);
    assert!(!path.exists());

    let ws = Workspace::create(
        &f.input("main.tex"),
        CompileOptions::default(),
        &f.config().with_keep(true),
    )
    .unwrap();
    assert!(ws.is_kept());
    let kept = ws.path().to_path_buf();
    drop(ws);
    assert!(kept.is_dir());

    let ws = f.create("main.tex").unwrap();
    let path = ws.path().to_path_buf();
    assert_eq!(ws.close().unwrap(), None);
    assert!(!path.exists());

    let mut ws = f.create("main.tex").unwrap();
    ws.set_keep(true);
    let path = ws.path().to_path_buf();
    assert_eq!(ws.close().unwrap(), Some(path.clone()));
    assert!(path.is_dir());
}

#[test]
fn workspace_below_project_root_is_not_copied_into_itself() {
    let f = Fixture::new();
    let parent = f.root().join("tmp");
    fs::create_dir(&parent).unwrap();
    let ws = Workspace::create(
        &f.input("main.tex"),
        CompileOptions::default(),
        &WorkspaceConfig::default().with_temp_parent(&parent),
    )
    .unwrap();
    assert!(!ws.path().join("tmp").exists());
    assert_eq!(
        ws.report()
            .excluded_with(ExclusionReason::WorkspaceDirectory)
            .count(),
        1
    );
}

// --- engine integration and artifact collection ----------------------------

/// A fake engine that writes `<stem>.pdf` (and a nested preview) into the
/// output directory, as a real engine would.
fn writing_engine() -> FakeEngine {
    FakeEngine::new(FakeBehavior::custom(|ctx, req| {
        let out = ctx.workspace.output_dir(&req.options);
        let src = fs::read_to_string(ctx.workspace.resolve(&req.entrypoint)).map_err(|e| {
            EngineError::Io {
                context: "reading entrypoint".into(),
                source: e,
            }
        })?;
        let pdf = FakeEngine::output_file(req, "pdf");
        fs::write(out.join(pdf.as_path()), format!("PDF of {src}")).unwrap();
        fs::create_dir_all(out.join("preview")).unwrap();
        fs::write(out.join("preview/page-1.png"), "PNG").unwrap();
        let mut r = CompileResult::new(
            CompileOutcome::Succeeded,
            EngineInfo::new(FakeEngine::NAME),
            Duration::from_millis(1),
        );
        r.artifacts.push(Artifact::new(ArtifactKind::Pdf, pdf));
        r.artifacts
            .push(Artifact::new(ArtifactKind::Preview, wp("preview/page-1.png")).with_page(1));
        Ok(r)
    }))
}

#[test]
fn engine_output_is_collected_to_the_host() {
    let f = Fixture::new();
    // A stale output of the same name in the input project is irrelevant.
    write(f.root(), "main.pdf", "stale input pdf");
    let dest = tempfile::tempdir().unwrap();

    let ws = f.create("main.tex").unwrap();
    let result = writing_engine()
        .compile(&ws.context(), ws.request())
        .unwrap();
    assert!(result.is_success());

    let collected = ws
        .collect_artifacts(&result.artifacts, dest.path(), OverwritePolicy::default())
        .unwrap();
    assert_eq!(collected[0].path.as_str(), "main.pdf");
    assert_eq!(collected[0].size_bytes, Some(30));
    assert_eq!(collected[1].path.as_str(), "preview/page-1.png");
    assert_eq!(collected[1].page, Some(1));
    assert_eq!(
        read(dest.path(), "main.pdf"),
        "PDF of \\documentclass{article}"
    );
    assert_eq!(read(dest.path(), "preview/page-1.png"), "PNG");
    // Only listed artifacts are collected; the project is untouched.
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 2);
    assert_eq!(read(f.root(), "main.pdf"), "stale input pdf");
}

#[test]
fn same_name_outputs() {
    let f = Fixture::new();
    let dest = tempfile::tempdir().unwrap();
    write(dest.path(), "main.pdf", "previous build");

    let ws = f.create("main.tex").unwrap();
    let result = writing_engine()
        .compile(&ws.context(), ws.request())
        .unwrap();
    let pdf_only = &result.artifacts[..1];

    // Refuse leaves the existing file alone.
    let err = ws
        .collect_artifacts(pdf_only, dest.path(), OverwritePolicy::Refuse)
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::OutputExists(_)), "{err:?}");
    assert_eq!(read(dest.path(), "main.pdf"), "previous build");

    // Replace (the default) overwrites it; duplicates are collected once.
    let twice = [pdf_only[0].clone(), pdf_only[0].clone()];
    let collected = ws
        .collect_artifacts(&twice, dest.path(), OverwritePolicy::Replace)
        .unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(
        read(dest.path(), "main.pdf"),
        "PDF of \\documentclass{article}"
    );
    // No temporary files are left behind.
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn collection_never_writes_through_symlinks() {
    let f = Fixture::new();
    let dest = tempfile::tempdir().unwrap();
    let victim = tempfile::tempdir().unwrap();
    write(victim.path(), "file", "precious");
    symlink(victim.path().join("file"), dest.path().join("main.pdf"));
    symlink(victim.path(), dest.path().join("preview"));

    let ws = f.create("main.tex").unwrap();
    let result = writing_engine()
        .compile(&ws.context(), ws.request())
        .unwrap();
    for artifact in &result.artifacts {
        let err = ws
            .collect_artifacts(
                std::slice::from_ref(artifact),
                dest.path(),
                OverwritePolicy::Replace,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkspaceError::UnsafeOutputPath(_)),
            "{err:?}"
        );
    }
    assert_eq!(read(victim.path(), "file"), "precious");
    assert_eq!(fs::read_dir(victim.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn collection_rejects_missing_and_non_regular_artifacts() {
    let f = Fixture::new();
    let dest = tempfile::tempdir().unwrap();
    let ws = f.create("main.tex").unwrap();

    let missing = [Artifact::new(ArtifactKind::Pdf, wp("main.pdf"))];
    assert!(matches!(
        ws.collect_artifacts(&missing, dest.path(), OverwritePolicy::Replace),
        Err(WorkspaceError::ArtifactMissing(_))
    ));

    // A symlink in the output directory pointing at an input file.
    symlink(ws.path().join("main.tex"), ws.output_dir().join("main.pdf"));
    assert!(matches!(
        ws.collect_artifacts(&missing, dest.path(), OverwritePolicy::Replace),
        Err(WorkspaceError::ArtifactNotFile(_))
    ));
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn engine_sees_only_the_workspace() {
    let f = Fixture::new();
    let engine = FakeEngine::new(FakeBehavior::Succeed);
    let ws = f.create("main.tex").unwrap();
    engine.compile(&ws.context(), ws.request()).unwrap();
    let calls = engine.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, ws.path());
    assert!(!calls[0].0.starts_with(f.root()));
    assert_eq!(&calls[0].1, ws.request());
}
