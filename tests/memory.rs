use std::path::Path;

use just_harness::api::Role;
use just_harness::memory::{
    ContextFile, ContextUpdate, INDEX_MAX_BYTES, INDEX_MAX_LINES, InstructionFiles, MAX_FILE_BYTES,
    Settings, context_update, index_excerpt, instruction_files, load_context, memory_dir,
    project_context,
};

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn names(dir: &Path, mode: InstructionFiles) -> Vec<String> {
    instruction_files(dir, Path::new(NO_HOME), mode)
        .iter()
        .filter_map(|p| p.strip_prefix(dir).ok())
        .map(|p| p.display().to_string())
        .collect()
}

/// The files read from `top` down for the working folder `top/cwd`, relative
/// to `top`. These tests hold only when no folder above the temporary folder
/// has an instruction file.
fn names_from(top: &Path, cwd: &str, home: &Path, mode: InstructionFiles) -> Vec<String> {
    instruction_files(&top.join(cwd), home, mode)
        .iter()
        .filter_map(|p| p.strip_prefix(top).ok())
        .map(|p| p.display().to_string())
        .collect()
}

const NO_HOME: &str = "/no-such-home";

fn writes(top: &Path, paths: &[&str]) {
    for path in paths {
        write(top, path, &format!("text of {path}"));
    }
}

fn from_root(top: &Path, cwd: &str, mode: InstructionFiles) -> Vec<String> {
    names_from(top, cwd, Path::new(NO_HOME), mode)
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

fn copy(path: &str, text: &str) -> ContextFile {
    ContextFile {
        path: path.into(),
        text: text.into(),
    }
}

fn update_text(update: &ContextUpdate) -> &str {
    let message = update.message.as_ref().unwrap();
    assert_eq!(message.role, Role::User);
    message.content[0].0["text"].as_str().unwrap()
}

#[test]
fn the_context_copies_are_the_files_as_read_and_the_index_excerpt() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(home.path(), ".just-harness/CLAUDE.md", "user rule\n");
    write(work.path(), "AGENTS.md", "project rule\n");
    let memory = memory_dir(home.path(), work.path());
    write(&memory, "MEMORY.md", "- [Role](role.md)\n");

    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    assert_eq!(
        loaded.files,
        [
            ContextFile {
                path: home
                    .path()
                    .join(".just-harness/CLAUDE.md")
                    .display()
                    .to_string(),
                text: "user rule".into()
            },
            ContextFile {
                path: work.path().join("AGENTS.md").display().to_string(),
                text: "project rule".into()
            },
            ContextFile {
                path: memory.join("MEMORY.md").display().to_string(),
                text: "- [Role](role.md)".into()
            },
        ]
    );
}

#[test]
fn a_blank_index_is_not_a_context_copy() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let memory = memory_dir(home.path(), work.path());
    write(&memory, "MEMORY.md", " \n");
    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    assert!(loaded.files.is_empty());
}

#[test]
fn the_index_copy_is_the_excerpt_sent_not_the_whole_file() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let memory = memory_dir(home.path(), work.path());
    let long: String = (0..INDEX_MAX_LINES + 5).map(|i| format!("{i}\n")).collect();
    write(&memory, "MEMORY.md", &long);
    let loaded = load_context(
        home.path(),
        work.path(),
        Some(&memory),
        InstructionFiles::default(),
    );
    assert_eq!(loaded.files[0].text, index_excerpt(&long).trim_end());
    assert_ne!(loaded.files[0].text, long);
}

#[test]
fn nothing_changed_gives_no_update() {
    let files = [copy("/w/CLAUDE.md", "rule")];
    assert_eq!(
        context_update(Some(&files), &files, None),
        ContextUpdate::default()
    );
    assert_eq!(context_update(None, &[], None), ContextUpdate::default());
}

#[test]
fn only_a_changed_file_is_sent() {
    let earlier = [
        copy("/w/CLAUDE.md", "old"),
        copy("/u/CLAUDE.md", "same rule"),
    ];
    let now = [
        copy("/u/CLAUDE.md", "same rule"),
        copy("/w/CLAUDE.md", "new"),
    ];
    let update = context_update(Some(&earlier), &now, None);

    assert_eq!(update.changed, ["/w/CLAUDE.md".to_string()]);
    assert!(update.gone.is_empty());
    let text = update_text(&update);
    assert!(text.contains("differ from their earlier copies"));
    assert!(text.contains("Contents of /w/CLAUDE.md:\n\nnew"));
    assert!(!text.contains("same rule") && !text.contains("old"));
    assert!(!text.contains("no longer present"));
}

#[test]
fn a_file_no_longer_there_is_named_and_alone_says_nothing_differs() {
    let earlier = [
        copy("/w/CLAUDE.md", "gone rule"),
        copy("/u/CLAUDE.md", "rule"),
    ];
    let now = [copy("/u/CLAUDE.md", "rule")];
    let update = context_update(Some(&earlier), &now, None);

    assert!(update.changed.is_empty());
    assert_eq!(update.gone, ["/w/CLAUDE.md".to_string()]);
    let text = update_text(&update);
    assert!(text.contains("Instructions no longer present: /w/CLAUDE.md"));
    assert!(!text.contains("differ from") && !text.contains("gone rule"));
}

#[test]
fn the_same_text_at_another_path_is_a_change() {
    let earlier = [copy("/a/CLAUDE.md", "rule")];
    let now = [copy("/b/CLAUDE.md", "rule")];
    let update = context_update(Some(&earlier), &now, None);

    assert_eq!(update.changed, ["/b/CLAUDE.md".to_string()]);
    assert_eq!(update.gone, ["/a/CLAUDE.md".to_string()]);
    let text = update_text(&update);
    assert!(
        text.find("Contents of /b/CLAUDE.md").unwrap() < text.find("no longer present").unwrap()
    );
}

#[test]
fn with_no_earlier_copies_the_files_come_as_a_first_project_context() {
    let now = [copy("/u/CLAUDE.md", "one"), copy("/w/CLAUDE.md", "two")];
    for earlier in [None, Some(&[][..])] {
        let update = context_update(earlier, &now, None);
        assert_eq!(
            update.changed,
            ["/u/CLAUDE.md".to_string(), "/w/CLAUDE.md".to_string()]
        );
        assert!(update.gone.is_empty());
        let text = update_text(&update);
        assert!(text.starts_with("<project-context>\n"));
        assert!(text.contains("Follow the instructions"));
        assert!(text.contains("Contents of /u/CLAUDE.md:\n\none"));
        assert!(text.contains("Contents of /w/CLAUDE.md:\n\ntwo"));
        assert!(!text.contains("earlier cop"));
    }
}

#[test]
fn an_update_says_the_earlier_instructions_still_apply() {
    let earlier = [copy("/w/CLAUDE.md", "old")];
    let now = [copy("/w/CLAUDE.md", "new")];
    let update = context_update(Some(&earlier), &now, None);
    let text = update_text(&update);
    assert!(text.starts_with("<project-context-update>\n"));
    assert!(text.contains("the instructions carried above still apply"));
}

fn loaded_files(home: &Path, work: &Path) -> Vec<ContextFile> {
    load_context(home, work, None, InstructionFiles::default()).files
}

#[test]
fn a_change_only_at_the_end_of_a_file_is_no_change() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "CLAUDE.md", "rule\n");
    let earlier = loaded_files(dir.path(), dir.path());
    write(dir.path(), "CLAUDE.md", "rule\n\n  \n");
    let now = loaded_files(dir.path(), dir.path());
    assert_eq!(
        context_update(Some(&earlier), &now, None),
        ContextUpdate::default()
    );
}

#[test]
fn a_blank_instruction_file_is_carried_by_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "CLAUDE.md", " \n");
    let loaded = load_context(dir.path(), dir.path(), None, InstructionFiles::default());
    assert!(loaded.files.is_empty());
    assert!(loaded.message.is_none());
}

#[test]
fn emptying_a_file_reads_as_no_longer_present() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "CLAUDE.md", "rule\n");
    let earlier = loaded_files(dir.path(), dir.path());
    write(dir.path(), "CLAUDE.md", "\n");
    let now = loaded_files(dir.path(), dir.path());
    let update = context_update(Some(&earlier), &now, None);
    assert!(update.changed.is_empty());
    assert_eq!(
        update.gone,
        [dir.path().join("CLAUDE.md").display().to_string()]
    );
}

#[test]
fn a_changed_index_is_named_as_the_memory_index() {
    let memory = Path::new("/m/memory");
    let now = [copy("/m/memory/MEMORY.md", "- a note")];
    let update = context_update(Some(&[]), &now, Some(memory));
    let text = update_text(&update);
    assert!(text.contains("Contents of /m/memory/MEMORY.md (your memory index; the files it names are in /m/memory):\n\n- a note"));

    // The index of another memory folder is an ordinary file here.
    let elsewhere = [copy("/other/memory/MEMORY.md", "- a note")];
    let update = context_update(Some(&[]), &elsewhere, Some(memory));
    assert!(update_text(&update).contains("Contents of /other/memory/MEMORY.md:\n\n- a note"));
}

#[test]
fn instruction_files_are_read_from_the_top_down_and_not_from_below() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(
        top,
        &[
            "CLAUDE.md",
            ".claude/CLAUDE.md",
            "CLAUDE.local.md",
            "a/CLAUDE.md",
            "a/b/CLAUDE.md",
            "a/b/CLAUDE.local.md",
            "a/b/c/CLAUDE.md",
        ],
    );
    assert_eq!(
        from_root(top, "a/b", InstructionFiles::default()),
        [
            "CLAUDE.md",
            ".claude/CLAUDE.md",
            "CLAUDE.local.md",
            "a/CLAUDE.md",
            "a/b/CLAUDE.md",
            "a/b/CLAUDE.local.md",
        ]
    );
}

#[test]
fn a_claude_md_anywhere_above_or_here_leaves_every_agents_md_out_by_default() {
    let above = tempfile::tempdir().unwrap();
    writes(above.path(), &["CLAUDE.md", "AGENTS.md", "a/AGENTS.md"]);
    assert_eq!(
        from_root(above.path(), "a", InstructionFiles::default()),
        ["CLAUDE.md"]
    );

    let here = tempfile::tempdir().unwrap();
    writes(here.path(), &["AGENTS.md", "a/CLAUDE.local.md"]);
    assert_eq!(
        from_root(here.path(), "a", InstructionFiles::default()),
        ["a/CLAUDE.local.md"]
    );

    let below = tempfile::tempdir().unwrap();
    writes(below.path(), &["AGENTS.md", "a/b/CLAUDE.md"]);
    assert_eq!(
        from_root(below.path(), "a", InstructionFiles::default()),
        ["AGENTS.md"],
        "a CLAUDE.md below the working folder does not count"
    );
}

#[test]
fn with_no_claude_md_every_agents_md_above_and_here_is_read() {
    let dir = tempfile::tempdir().unwrap();
    writes(
        dir.path(),
        &["AGENTS.md", ".claude/AGENTS.md", "a/AGENTS.md"],
    );
    assert_eq!(
        from_root(dir.path(), "a", InstructionFiles::default()),
        ["AGENTS.md", ".claude/AGENTS.md", "a/AGENTS.md"]
    );
}

#[test]
fn the_other_modes_apply_at_every_folder() {
    let dir = tempfile::tempdir().unwrap();
    writes(
        dir.path(),
        &["CLAUDE.md", "AGENTS.md", "a/CLAUDE.md", "a/AGENTS.md"],
    );
    assert_eq!(
        from_root(dir.path(), "a", InstructionFiles::ClaudeMdAndAgentsMd),
        ["CLAUDE.md", "AGENTS.md", "a/CLAUDE.md", "a/AGENTS.md"]
    );
    assert_eq!(
        from_root(dir.path(), "a", InstructionFiles::ClaudeMd),
        ["CLAUDE.md", "a/CLAUDE.md"]
    );
}

#[test]
fn a_git_repository_is_no_boundary() {
    let dir = tempfile::tempdir().unwrap();
    writes(dir.path(), &["CLAUDE.md", "repo/CLAUDE.md"]);
    std::fs::create_dir(dir.path().join("repo/.git")).unwrap();
    assert_eq!(
        from_root(dir.path(), "repo", InstructionFiles::default()),
        ["CLAUDE.md", "repo/CLAUDE.md"]
    );
}

#[test]
fn the_home_folders_dot_claude_is_not_read_and_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(
        top,
        &[
            "home/CLAUDE.local.md",
            "home/.claude/CLAUDE.md",
            "home/.claude/AGENTS.md",
            "home/p/.claude/CLAUDE.md",
        ],
    );
    assert_eq!(
        names_from(
            top,
            "home/p",
            &top.join("home"),
            InstructionFiles::default()
        ),
        ["home/CLAUDE.local.md", "home/p/.claude/CLAUDE.md"]
    );

    let agents = tempfile::tempdir().unwrap();
    let top = agents.path();
    writes(top, &["home/.claude/CLAUDE.md", "home/p/AGENTS.md"]);
    assert_eq!(
        names_from(
            top,
            "home/p",
            &top.join("home"),
            InstructionFiles::default()
        ),
        ["home/p/AGENTS.md"]
    );
}

#[test]
fn the_home_folder_is_recognised_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(top, &["home/.claude/CLAUDE.md", "home/p/CLAUDE.md"]);
    std::os::unix::fs::symlink(top.join("home"), top.join("link")).unwrap();
    assert_eq!(
        names_from(
            top,
            "home/p",
            &top.join("link"),
            InstructionFiles::default()
        ),
        ["home/p/CLAUDE.md"]
    );
}

fn text_of(top: &Path, cwd: &str) -> (String, Vec<std::path::PathBuf>) {
    let home = tempfile::tempdir().unwrap();
    let loaded = load_context(
        home.path(),
        &top.join(cwd),
        None,
        InstructionFiles::default(),
    );
    let skipped = loaded.skipped.iter().map(|(p, _)| p.clone()).collect();
    (
        loaded
            .message
            .map(|m| m.content[0].0["text"].as_str().unwrap().to_string())
            .unwrap_or_default(),
        skipped,
    )
}

#[test]
fn a_file_above_is_held_to_the_folder_it_was_found_in() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(top, &["outside/secret", "t/a/CLAUDE.md", "t/kept.md"]);
    write(top, "t/.env", "SECRET=1");
    std::fs::create_dir_all(top.join("t/.claude")).unwrap();
    std::os::unix::fs::symlink(top.join("outside/secret"), top.join("t/CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink(top.join("t/.env"), top.join("t/CLAUDE.local.md")).unwrap();
    std::os::unix::fs::symlink(top.join("t/kept.md"), top.join("t/.claude/CLAUDE.md")).unwrap();

    let (text, skipped) = text_of(top, "t/a");
    assert!(
        text.contains("text of t/kept.md"),
        "a link inside that folder"
    );
    assert!(text.contains("text of t/a/CLAUDE.md"));
    assert!(
        !text.contains("outside/secret"),
        "a link out of that folder"
    );
    assert!(!text.contains("SECRET"), "a link to a .env file");
    assert_eq!(
        skipped,
        [top.join("t/CLAUDE.md"), top.join("t/CLAUDE.local.md")]
    );
}

#[test]
fn a_file_here_may_not_lead_to_the_folder_above() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(top, &["t/shared.md"]);
    std::fs::create_dir_all(top.join("t/a")).unwrap();
    std::os::unix::fs::symlink(top.join("t/shared.md"), top.join("t/a/CLAUDE.md")).unwrap();
    let (text, skipped) = text_of(top, "t/a");
    assert!(!text.contains("text of t/shared.md"));
    assert_eq!(skipped, [top.join("t/a/CLAUDE.md")]);
}

#[test]
fn a_file_reached_from_two_folders_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(top, &["t/a/CLAUDE.md"]);
    std::os::unix::fs::symlink(top.join("t/a/CLAUDE.md"), top.join("t/CLAUDE.md")).unwrap();
    let (text, _) = text_of(top, "t/a");
    assert_eq!(text.matches("text of t/a/CLAUDE.md").count(), 1);
}

#[test]
fn resuming_in_a_subfolder_keeps_the_files_above() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    let home = tempfile::tempdir().unwrap();
    writes(top, &["r/CLAUDE.md"]);
    std::fs::create_dir_all(top.join("r/sub")).unwrap();
    let load = |cwd: &str| {
        load_context(
            home.path(),
            &top.join(cwd),
            None,
            InstructionFiles::default(),
        )
        .files
    };
    let earlier = load("r");
    assert_eq!(
        context_update(Some(&earlier), &load("r/sub"), None),
        ContextUpdate::default()
    );

    write(top, "r/sub/CLAUDE.md", "the subfolder's");
    let update = context_update(Some(&earlier), &load("r/sub"), None);
    assert_eq!(
        update.changed,
        [top.join("r/sub/CLAUDE.md").display().to_string()]
    );
    assert!(update.gone.is_empty());
}

#[test]
fn a_file_that_carries_nothing_above_does_not_stop_an_agents_md() {
    let blank = tempfile::tempdir().unwrap();
    write(blank.path(), "CLAUDE.md", " \n");
    write(blank.path(), "a/AGENTS.md", "agents");
    assert_eq!(
        from_root(blank.path(), "a", InstructionFiles::default()),
        ["CLAUDE.md", "a/AGENTS.md"],
        "listed, and left out when read"
    );

    let large = tempfile::tempdir().unwrap();
    let too_big = "x".repeat(MAX_FILE_BYTES as usize + 1);
    write(large.path(), "CLAUDE.md", &too_big);
    write(large.path(), "a/AGENTS.md", "agents");
    assert_eq!(
        from_root(large.path(), "a", InstructionFiles::default()),
        ["CLAUDE.md", "a/AGENTS.md"]
    );

    let linked = tempfile::tempdir().unwrap();
    let top = linked.path();
    writes(top, &["outside/secret", "t/a/AGENTS.md"]);
    std::os::unix::fs::symlink(top.join("outside/secret"), top.join("t/CLAUDE.md")).unwrap();
    assert_eq!(
        from_root(top, "t/a", InstructionFiles::default()),
        ["t/CLAUDE.md", "t/a/AGENTS.md"]
    );
}

#[test]
fn a_user_file_that_carries_nothing_does_not_stop_the_users_agents_md() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write(home.path(), ".just-harness/CLAUDE.md", "\n");
    write(home.path(), ".just-harness/AGENTS.md", "user agents");
    let loaded = load_context(home.path(), work.path(), None, InstructionFiles::default());
    assert_eq!(loaded.files.len(), 1);
    assert_eq!(loaded.files[0].text, "user agents");
}

#[test]
fn inside_the_home_folders_dot_claude_that_folder_is_not_read_either() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(
        top,
        &["home/.claude/CLAUDE.md", "home/.claude/skills/x/AGENTS.md"],
    );
    assert_eq!(
        names_from(
            top,
            "home/.claude/skills/x",
            &top.join("home"),
            InstructionFiles::default()
        ),
        ["home/.claude/skills/x/AGENTS.md"]
    );

    let linked = tempfile::tempdir().unwrap();
    let top = linked.path();
    writes(top, &["dot/claude/CLAUDE.md", "dot/claude/s/CLAUDE.md"]);
    std::fs::create_dir(top.join("home")).unwrap();
    std::os::unix::fs::symlink(top.join("dot/claude"), top.join("home/.claude")).unwrap();
    assert_eq!(
        names_from(
            top,
            "dot/claude/s",
            &top.join("home"),
            InstructionFiles::default()
        ),
        ["dot/claude/s/CLAUDE.md"]
    );
}

/// Makes `top/main` a repository with a worktree at `top/<worktree>`, in the
/// shape `git worktree add` leaves.
fn with_worktree(top: &Path, git_folder: &str, worktree: &str) {
    let name = "wt";
    let admin = top.join(git_folder).join("worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(top.join(worktree)).unwrap();
    std::fs::write(
        top.join(worktree).join(".git"),
        format!("gitdir: {}\n", admin.canonicalize().unwrap().display()),
    )
    .unwrap();
}

#[test]
fn above_a_worktree_inside_its_repository_only_the_local_file_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(
        top,
        &[
            "CLAUDE.md",
            "r/CLAUDE.md",
            "r/.claude/CLAUDE.md",
            "r/CLAUDE.local.md",
            "r/a/CLAUDE.md",
            "r/a/CLAUDE.local.md",
            "r/a/wt/CLAUDE.md",
            "r/a/wt/s/CLAUDE.md",
        ],
    );
    with_worktree(top, "r/.git", "r/a/wt");
    assert_eq!(
        from_root(top, "r/a/wt/s", InstructionFiles::default()),
        [
            "CLAUDE.md",
            "r/CLAUDE.local.md",
            "r/a/CLAUDE.local.md",
            "r/a/wt/CLAUDE.md",
            "r/a/wt/s/CLAUDE.md",
        ]
    );
}

#[test]
fn a_main_checkouts_claude_md_does_not_stop_a_worktrees_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    writes(top, &["r/CLAUDE.md", "r/AGENTS.md", "r/wt/AGENTS.md"]);
    with_worktree(top, "r/.git", "r/wt");
    assert_eq!(
        from_root(top, "r/wt", InstructionFiles::default()),
        ["r/wt/AGENTS.md"]
    );

    let local = tempfile::tempdir().unwrap();
    let top = local.path();
    writes(top, &["r/CLAUDE.local.md", "r/wt/AGENTS.md"]);
    with_worktree(top, "r/.git", "r/wt");
    assert_eq!(
        from_root(top, "r/wt", InstructionFiles::default()),
        ["r/CLAUDE.local.md"],
        "the local file above still counts"
    );
}

#[test]
fn a_worktree_outside_its_main_checkout_reads_the_folders_above_as_usual() {
    let beside = tempfile::tempdir().unwrap();
    let top = beside.path();
    writes(top, &["CLAUDE.md", "r/CLAUDE.md", "wt/CLAUDE.md"]);
    with_worktree(top, "r/.git", "wt");
    assert_eq!(
        from_root(top, "wt", InstructionFiles::default()),
        ["CLAUDE.md", "wt/CLAUDE.md"]
    );

    let bare = tempfile::tempdir().unwrap();
    let top = bare.path();
    writes(top, &["b/CLAUDE.md", "b/main/CLAUDE.md"]);
    with_worktree(top, "b/repo.git", "b/main");
    assert_eq!(
        from_root(top, "b/main", InstructionFiles::default()),
        ["b/CLAUDE.md", "b/main/CLAUDE.md"]
    );
}
