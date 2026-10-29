use std::path::Path;

use just_harness::api::Role;
use just_harness::memory::{
    INDEX_MAX_BYTES, INDEX_MAX_LINES, InstructionFiles, MAX_FILE_BYTES, Settings, index_excerpt,
    instruction_files, load_context, memory_dir, project_context,
};

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn names(dir: &Path, mode: InstructionFiles) -> Vec<String> {
    instruction_files(dir, mode)
        .iter()
        .map(|p| p.strip_prefix(dir).unwrap().display().to_string())
        .collect()
}

#[test]
fn the_modes_are_named_as_in_claude_code() {
    assert_eq!(
        InstructionFiles::parse("claude-md-or-agents-md"),
        Some(InstructionFiles::ClaudeMdOrAgentsMd)
    );
    assert_eq!(
        InstructionFiles::parse("claude-md-and-agents-md"),
        Some(InstructionFiles::ClaudeMdAndAgentsMd)
    );
    assert_eq!(
        InstructionFiles::parse("claude-md"),
        Some(InstructionFiles::ClaudeMd)
    );
    assert_eq!(InstructionFiles::parse("agents"), None);
    assert_eq!(
        InstructionFiles::default(),
        InstructionFiles::ClaudeMdOrAgentsMd
    );
}

#[test]
fn agents_md_is_read_only_where_no_claude_md_is_by_default() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "AGENTS.md", "a");
    write(dir.path(), ".claude/AGENTS.md", "b");
    assert_eq!(
        names(dir.path(), InstructionFiles::ClaudeMdOrAgentsMd),
        ["AGENTS.md", ".claude/AGENTS.md"]
    );

    write(dir.path(), "CLAUDE.local.md", "c");
    assert_eq!(
        names(dir.path(), InstructionFiles::ClaudeMdOrAgentsMd),
        ["CLAUDE.local.md"]
    );
}

#[test]
fn each_mode_reads_its_files_claude_md_first() {
    let dir = tempfile::tempdir().unwrap();
    for f in [
        "CLAUDE.md",
        ".claude/CLAUDE.md",
        "CLAUDE.local.md",
        "AGENTS.md",
        ".claude/AGENTS.md",
    ] {
        write(dir.path(), f, "x");
    }
    let claude = ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"];
    assert_eq!(
        names(dir.path(), InstructionFiles::ClaudeMdOrAgentsMd),
        claude
    );
    assert_eq!(names(dir.path(), InstructionFiles::ClaudeMd), claude);
    assert_eq!(
        names(dir.path(), InstructionFiles::ClaudeMdAndAgentsMd),
        [&claude[..], &["AGENTS.md", ".claude/AGENTS.md"]].concat()
    );
    let empty = tempfile::tempdir().unwrap();
    write(empty.path(), "AGENTS.md", "a");
    assert!(names(empty.path(), InstructionFiles::ClaudeMd).is_empty());
}

#[test]
fn settings_come_from_the_file_and_default_when_it_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    assert_eq!(Settings::load(&path).unwrap(), Settings::default());

    std::fs::write(
        &path,
        r#"{ "instructionFiles": "claude-md-and-agents-md" }"#,
    )
    .unwrap();
    assert_eq!(
        Settings::load(&path).unwrap().instruction_files,
        InstructionFiles::ClaudeMdAndAgentsMd
    );

    std::fs::write(&path, r#"{ "instructionFiles": "both" }"#).unwrap();
    let error = Settings::load(&path).unwrap_err();
    assert!(
        error.contains("instructionFiles") && error.contains("both"),
        "{error}"
    );

    std::fs::write(&path, "not json").unwrap();
    assert!(Settings::load(&path).is_err());
}

#[test]
fn memory_is_kept_per_working_folder_under_home() {
    assert_eq!(
        memory_dir(Path::new("/home/me"), Path::new("/work/project")),
        Path::new("/home/me/.just-harness/projects/-work-project/memory")
    );
}

#[test]
fn the_index_is_cut_at_its_line_or_byte_limit() {
    let lines: String = (0..INDEX_MAX_LINES + 5)
        .map(|i| format!("- line {i}\n"))
        .collect();
    let cut = index_excerpt(&lines);
    assert_eq!(cut.lines().count(), INDEX_MAX_LINES);
    assert!(lines.starts_with(cut));

    let long = "é".repeat(INDEX_MAX_BYTES);
    let cut = index_excerpt(&long);
    assert!(cut.len() <= INDEX_MAX_BYTES && cut.len() > INDEX_MAX_BYTES - 2);
    assert_eq!(index_excerpt("- one\n- two"), "- one\n- two");
}

#[test]
fn the_context_names_each_file_and_the_memory_index() {
    let files = vec![
        (
            Path::new("/home/me/.just-harness/CLAUDE.md").to_path_buf(),
            "Talk briefly.".to_string(),
        ),
        (
            Path::new("/work/AGENTS.md").to_path_buf(),
            "Run cargo test.".to_string(),
        ),
    ];
    let message = project_context(&files, Some((Path::new("/mem"), "- [Role](role.md)"))).unwrap();
    assert_eq!(message.role, Role::User);
    assert_eq!(message.content.len(), 1);
    let text = message.content[0].0["text"].as_str().unwrap();
    let user = text.find("/home/me/.just-harness/CLAUDE.md").unwrap();
    let project = text.find("/work/AGENTS.md").unwrap();
    let memory = text.find("/mem/MEMORY.md").unwrap();
    assert!(user < text.find("Talk briefly.").unwrap());
    assert!(text.find("Talk briefly.").unwrap() < project);
    assert!(project < text.find("Run cargo test.").unwrap());
    assert!(text.find("Run cargo test.").unwrap() < memory);
    assert!(memory < text.find("- [Role](role.md)").unwrap());

    assert!(project_context(&[], None).is_none());
    assert!(project_context(&[], Some((Path::new("/mem"), "  \n"))).is_none());
}

#[test]
fn loading_reads_user_then_project_files_and_the_index() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(home.path(), ".just-harness/CLAUDE.md", "user rule");
    write(work.path(), "AGENTS.md", "project rule");
    let memory = memory_dir(home.path(), work.path());
    write(&memory, "MEMORY.md", "- [Role](role.md)");

    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    let text = context_text(&loaded);
    assert!(text.find("user rule").unwrap() < text.find("project rule").unwrap());
    assert!(text.contains("- [Role](role.md)"));
    assert_eq!(loaded.read.len(), 3);
    assert!(loaded.skipped.is_empty());

    let without = load_context(home.path(), work.path(), None, InstructionFiles::default());
    assert!(!context_text(&without).contains("- [Role](role.md)"));

    let bare = tempfile::tempdir().unwrap();
    let nothing = load_context(bare.path(), bare.path(), None, InstructionFiles::default());
    assert!(nothing.message.is_none());
}

#[test]
fn an_instruction_file_over_the_size_limit_is_skipped() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(
        work.path(),
        "CLAUDE.md",
        &"x".repeat(MAX_FILE_BYTES as usize + 1),
    );
    write(work.path(), "CLAUDE.local.md", "small rule");

    let loaded = load_context(home.path(), work.path(), None, InstructionFiles::default());
    let text = context_text(&loaded);
    assert!(text.contains("small rule"));
    assert!(!text.contains("xxxx"));
    assert_eq!(loaded.skipped.len(), 1);
    assert!(loaded.skipped[0].0.ends_with("CLAUDE.md"));
}

#[test]
fn an_instruction_file_linking_outside_its_folder_or_to_a_secret_is_not_read() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(home.path(), "secret.txt", "TOP SECRET");
    write(work.path(), ".env", "KEY=sk-test");
    std::os::unix::fs::symlink(
        home.path().join("secret.txt"),
        work.path().join("CLAUDE.md"),
    )
    .unwrap();
    std::fs::create_dir_all(work.path().join(".claude")).unwrap();
    std::os::unix::fs::symlink(
        work.path().join(".env"),
        work.path().join(".claude/CLAUDE.md"),
    )
    .unwrap();
    write(work.path(), "CLAUDE.local.md", "local rule");
    let memory = home.path().join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    std::os::unix::fs::symlink(home.path().join("secret.txt"), memory.join("MEMORY.md")).unwrap();

    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    let text = context_text(&loaded);
    assert!(text.contains("local rule"));
    assert!(
        !text.contains("TOP SECRET") && !text.contains("sk-test"),
        "{text}"
    );
    assert_eq!(loaded.skipped.len(), 3, "{:?}", loaded.skipped);
}

#[test]
fn a_file_linked_under_two_names_is_read_once() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(work.path(), "AGENTS.md", "shared rule");
    std::os::unix::fs::symlink("AGENTS.md", work.path().join("CLAUDE.md")).unwrap();

    let loaded = load_context(
        home.path(),
        work.path(),
        None,
        InstructionFiles::ClaudeMdAndAgentsMd,
    );
    assert_eq!(context_text(&loaded).matches("shared rule").count(), 1);
    assert_eq!(loaded.read.len(), 1);
}

#[test]
fn an_unreadable_file_is_skipped_and_the_rest_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(work.path(), "CLAUDE.md", "locked rule");
    std::fs::set_permissions(
        work.path().join("CLAUDE.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    write(work.path(), "CLAUDE.local.md", "open rule");
    let memory = home.path().join("memory");
    write(&memory, "MEMORY.md", "- [Role](role.md)");

    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    let text = context_text(&loaded);
    assert!(text.contains("open rule") && text.contains("- [Role](role.md)"));
    assert!(!text.contains("locked rule"));
    assert_eq!(loaded.skipped.len(), 1);
}

fn context_text(loaded: &just_harness::memory::Loaded) -> String {
    loaded
        .message
        .as_ref()
        .map(|m| m.content[0].0["text"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

#[test]
fn memory_belongs_to_the_git_repository_the_folder_is_in() {
    let home = Path::new("/home/me");
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git/worktrees/feature")).unwrap();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join(".git/worktrees/feature/commondir"), "../..\n").unwrap();
    let at_root = memory_dir(home, &root);
    assert_eq!(at_root, memory_dir(home, &root.join("src/deep")));
    assert!(at_root.ends_with(format!(
        "{}/memory",
        root.to_string_lossy().replace('/', "-")
    )));

    let worktree = tempfile::tempdir().unwrap();
    let tree = worktree.path().canonicalize().unwrap();
    std::fs::write(
        tree.join(".git"),
        format!(
            "gitdir: {}\n",
            root.join(".git/worktrees/feature").display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    assert_eq!(memory_dir(home, &tree.join("sub")), at_root);

    let plain = tempfile::tempdir().unwrap();
    let plain = plain.path().canonicalize().unwrap();
    assert!(memory_dir(home, &plain).ends_with(format!(
        "{}/memory",
        plain.to_string_lossy().replace('/', "-")
    )));
}
