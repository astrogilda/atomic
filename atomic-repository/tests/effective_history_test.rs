//! Integration tests for `Repository::effective_history`.
//!
//! `effective_history` returns the *full* change set a view depends on, in
//! dependency order — the draft's own changes plus everything inherited from
//! its shared base. This is what `push` uploads so a flattened (shared)
//! remote view receives a complete graph, unlike `log`, which for a draft
//! shows only that view's own new changes.

use std::fs;
use std::path::Path;

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::types::Hash;
use atomic_repository::history::HistoryOptions;
use atomic_repository::{RecordOptions, Repository};
use tempfile::TempDir;

fn add_file(repo: &Repository, repo_path: &Path, name: &str, content: &str) {
    fs::write(repo_path.join(name), content).expect("write file");
    repo.add(name, Default::default()).expect("add file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(header, RecordOptions::default())
        .expect("record")
        .hash()
}

/// A draft view's `effective_history` must include its shared base's changes,
/// ordered base-first, while `log` on the draft shows only its own changes.
#[test]
fn effective_history_includes_inherited_base_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    // Change A on the shared base view (dev).
    add_file(&repo, &repo_path, "a.txt", "A\n");
    let a = record(&repo, "add a");

    // Create a draft view parented on dev and switch to it.
    repo.create_view("feature").expect("create draft view");
    repo.switch_view("feature").expect("switch to draft");

    // Change B recorded on the draft.
    add_file(&repo, &repo_path, "b.txt", "B\n");
    let b = record(&repo, "add b");

    // `log` on the draft shows only its own new change.
    let own: Vec<Hash> = repo
        .log(HistoryOptions::default().view("feature"))
        .expect("log draft")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    assert_eq!(own, vec![b], "draft log should show only its own change");

    // `effective_history` includes the inherited base change first.
    let effective: Vec<Hash> = repo
        .effective_history(Some("feature"))
        .expect("effective history")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    assert_eq!(
        effective,
        vec![a, b],
        "effective history should be base-first: inherited change then own change"
    );
}

/// For a shared view, `effective_history` equals `log` (shared views are
/// self-contained) — this guards against changing push behavior for shared
/// views.
#[test]
fn effective_history_matches_log_for_shared_view() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "a.txt", "A\n");
    let a = record(&repo, "add a");
    add_file(&repo, &repo_path, "b.txt", "B\n");
    let b = record(&repo, "add b");

    let log_hashes: Vec<Hash> = repo
        .log(HistoryOptions::default())
        .expect("log")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    let effective: Vec<Hash> = repo
        .effective_history(None)
        .expect("effective history")
        .into_iter()
        .map(|e| e.hash)
        .collect();

    assert_eq!(log_hashes, vec![a, b]);
    assert_eq!(effective, log_hashes);
}

/// A nested draft inherits both history and content through every parent,
/// without duplicating changes copied into a draft's own log.
#[test]
fn nested_draft_effective_history_and_content_include_all_parents() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "base.txt", "base\n");
    let base = record(&repo, "base");

    let shared_view = repo.current_view().to_string();
    repo.create_view_from("parent", &shared_view)
        .expect("create parent draft");
    repo.switch_view("parent").expect("switch to parent draft");
    add_file(&repo, &repo_path, "parent.txt", "parent\n");
    let parent = record(&repo, "parent");

    repo.create_view_from("child", "parent")
        .expect("create nested child draft");
    repo.switch_view("child").expect("switch to child draft");
    add_file(&repo, &repo_path, "child.txt", "child\n");
    let child = record(&repo, "child");

    let effective: Vec<Hash> = repo
        .effective_history(Some("child"))
        .expect("nested effective history")
        .into_iter()
        .map(|entry| entry.hash)
        .collect();
    assert_eq!(effective, vec![base, parent, child]);

    assert_eq!(
        repo.get_file_content_on_view("base.txt", "child")
            .expect("read inherited base content"),
        Some(b"base\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("parent.txt", "child")
            .expect("read inherited parent content"),
        Some(b"parent\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("child.txt", "child")
            .expect("read child content"),
        Some(b"child\n".to_vec())
    );
}

/// A tracked empty file is distinct from an untracked path: its recorded
/// content is present and has zero bytes.
#[test]
fn tracked_empty_file_content_is_some_empty_vec() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "empty.txt", "");
    record(&repo, "add empty file");

    assert_eq!(
        repo.get_file_content("empty.txt")
            .expect("read tracked empty file"),
        Some(Vec::new())
    );
}
