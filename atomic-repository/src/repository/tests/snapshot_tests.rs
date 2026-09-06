use atomic_core::change::ChangeKind;
use atomic_core::operation::OperationScope;

use super::*;

fn record_options() -> RecordOptions {
    RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true)
}

fn direct_view_hashes(repo: &Repository, view_name: &str) -> Vec<Hash> {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(view_name).unwrap().unwrap();
    txn.iter_changes(&view, 0)
        .unwrap()
        .map(|row| {
            let (_, node_id, _) = row.unwrap();
            txn.get_external(node_id).unwrap().unwrap()
        })
        .collect()
}

#[test]
fn snapshot_replacement_is_baseline_relative_private_and_journaled() {
    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("snapshot.txt");
    std::fs::write(&path, b"durable baseline\n").unwrap();
    repo.add("snapshot.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"snapshot one\n").unwrap();
    let first = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot one"),
            record_options(),
        )
        .unwrap();
    let first_hash = *first.hash();
    assert_eq!(
        first.change().kind(),
        &ChangeKind::Snapshot { working_copy }
    );
    assert!(first.change().supersedes().is_none());
    assert!(first.change().dependencies().iter().all(|dependency| repo
        .load_change(dependency)
        .unwrap()
        .kind()
        .is_durable()));

    let snapshot_view = Repository::snapshot_view_name(working_copy);
    let info = repo.get_view_info(&snapshot_view).unwrap();
    assert!(info.scope.is_draft());
    assert_eq!(info.parent_name.as_deref(), Some("dev"));
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), vec![first_hash]);
    assert!(repo
        .log(HistoryOptions::default().view(snapshot_view.clone()))
        .unwrap()
        .is_empty());
    assert!(repo.view_manifest(&snapshot_view).is_err());
    assert!(repo
        .insert_change(&first_hash, InsertOptions::default().view("dev"),)
        .is_err());
    repo.create_draft_view("other-private", "dev").unwrap();
    assert!(repo
        .insert_change(&first_hash, InsertOptions::default().view("other-private"),)
        .is_err());
    assert!(repo
        .set_view_scope(&snapshot_view, ViewScope::Shared)
        .is_err());

    std::fs::write(&path, b"snapshot two\n").unwrap();
    let second = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot two"),
            record_options(),
        )
        .unwrap();
    let second_hash = *second.hash();
    assert_eq!(second.change().supersedes(), Some(&first_hash));
    assert!(!second.change().dependencies().contains(&first_hash));
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), vec![second_hash]);

    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(
        details.operation.payload().delta.metadata.len(),
        2,
        "replacement must journal one removal and one insertion lease"
    );
}

#[test]
fn promotion_reassembles_identical_content_without_snapshot_dependency() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("promote.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("promote.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"promoted content\n").unwrap();
    let snapshot = repo
        .snapshot(
            working_copy,
            ChangeHeader::new("snapshot"),
            record_options(),
        )
        .unwrap();
    let snapshot_hash = *snapshot.hash();
    let promoted = repo
        .promote_snapshot(
            working_copy,
            ChangeHeader::new("promote snapshot"),
            record_options(),
        )
        .unwrap();

    assert!(promoted.change().kind().is_durable());
    assert!(promoted.change().supersedes().is_none());
    assert!(!promoted.change().dependencies().contains(&snapshot_hash));
    assert_eq!(promoted.change().hunks(), snapshot.change().hunks());
    assert_eq!(promoted.change().contents, snapshot.change().contents);
    assert_eq!(
        promoted.change().hashed.file_ops,
        snapshot.change().hashed.file_ops
    );

    let snapshot_view = Repository::snapshot_view_name(working_copy);
    assert!(direct_view_hashes(&repo, &snapshot_view).is_empty());
    assert!(direct_view_hashes(&repo, "dev").contains(promoted.hash()));
    assert!(repo.status(StatusOptions::default()).unwrap().is_clean());
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.operation.payload().delta.metadata.len(), 2);
}

#[test]
fn split_snapshot_materializes_index_and_remainder_without_snapshot_dependencies() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("split.txt");
    std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
    repo.add("split.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"one\nTWO\nTHREE\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();

    let outcome = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![IndexManifestEntry::present(
                "split.txt",
                b"one\nTWO\nthree\n".to_vec(),
            )]),
        )
        .unwrap();

    assert_eq!(
        repo.get_file_content_on_view("split.txt", "dev")
            .unwrap()
            .unwrap(),
        b"one\nTWO\nthree\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("split.txt", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        b"one\nTWO\nTHREE\n"
    );
    let lifecycle = repo.snapshot_status(working_copy).unwrap();
    assert_eq!(lifecycle.snapshot, None);
    assert_eq!(lifecycle.remainder, Some(outcome.remainder));
    let index = repo.load_change(&outcome.index).unwrap();
    let remainder = repo.load_change(&outcome.remainder).unwrap();
    assert!(index.kind().is_durable());
    assert!(remainder.kind().is_durable());
    assert!(remainder.dependencies().contains(&outcome.index));
    for dependency in index.dependencies().iter().chain(remainder.dependencies()) {
        assert!(!repo.load_change(dependency).unwrap().kind().is_snapshot());
    }
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .unwrap();
    let head = match log.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repo.operation_details(head).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.operation.payload().delta.metadata.len(), 3);
}

#[cfg(unix)]
#[test]
fn split_snapshot_preserves_rename_edit_and_mode_only_state() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    std::fs::write(directory.path().join("old.txt"), b"base\n").unwrap();
    std::fs::write(directory.path().join("mode.txt"), b"mode\n").unwrap();
    std::fs::write(directory.path().join("unstaged.txt"), b"base\n").unwrap();
    repo.add_batch(&["old.txt", "mode.txt", "unstaged.txt"])
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::rename(
        directory.path().join("old.txt"),
        directory.path().join("new.txt"),
    )
    .unwrap();
    std::fs::write(directory.path().join("new.txt"), b"renamed and edited\n").unwrap();
    std::fs::write(directory.path().join("unstaged.txt"), b"worktree\n").unwrap();
    std::fs::set_permissions(
        directory.path().join("mode.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("rename snapshot"),
        record_options(),
    )
    .unwrap();

    let manifest = IndexManifest::new(vec![
        IndexManifestEntry {
            path: "new.txt".to_string(),
            source_path: Some("old.txt".to_string()),
            state: IndexEntryState::Present {
                repository_bytes: Some(b"renamed and edited\n".to_vec()),
                mode: 0o644,
                kind: atomic_core::change::InodeKind::Regular,
            },
        },
        IndexManifestEntry {
            path: "mode.txt".to_string(),
            source_path: None,
            state: IndexEntryState::Present {
                repository_bytes: None,
                mode: 0o755,
                kind: atomic_core::change::InodeKind::Regular,
            },
        },
    ]);
    let outcome = repo.split_snapshot(working_copy, manifest).unwrap();

    assert!(repo
        .get_file_content_on_view("old.txt", "dev")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("new.txt", "dev")
            .unwrap()
            .unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("unstaged.txt", "dev")
            .unwrap()
            .unwrap(),
        b"base\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("unstaged.txt", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        b"worktree\n"
    );
    repo.materialize().unwrap();
    assert!(!directory.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read(directory.path().join("new.txt")).unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("unstaged.txt")).unwrap(),
        b"base\n"
    );
    assert_eq!(
        std::fs::metadata(directory.path().join("mode.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );

    repo.switch_view(&outcome.snapshot_view).unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("new.txt")).unwrap(),
        b"renamed and edited\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("unstaged.txt")).unwrap(),
        b"worktree\n"
    );
}

#[test]
fn split_snapshot_preserves_filter_and_opaque_binary_repository_bytes() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let git_ok = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(directory.path())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !git_ok {
        return;
    }
    std::fs::write(
        directory.path().join(".gitattributes"),
        b"filtered.txt text eol=crlf\nbinary.dat binary\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("filtered.txt"), b"base\r\n").unwrap();
    std::fs::write(directory.path().join("binary.dat"), b"base\0binary").unwrap();
    repo.add_batch(&[".gitattributes", "filtered.txt", "binary.dat"])
        .unwrap();
    assert!(std::process::Command::new("git")
        .args(["add", ".gitattributes", "filtered.txt", "binary.dat"])
        .current_dir(directory.path())
        .status()
        .unwrap()
        .success());
    repo.record(ChangeHeader::new("filtered baseline"), record_options())
        .unwrap();

    let final_binary = b"worktree\0binary\xff".to_vec();
    std::fs::write(directory.path().join("filtered.txt"), b"one\r\ntwo\r\n").unwrap();
    std::fs::write(directory.path().join("binary.dat"), &final_binary).unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("filtered snapshot"),
        record_options(),
    )
    .unwrap();
    let outcome = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![
                IndexManifestEntry::present("filtered.txt", b"one\ntwo\n".to_vec()),
                IndexManifestEntry::present("binary.dat", b"index\0binary\xfe".to_vec()),
            ]),
        )
        .unwrap();

    assert_eq!(
        repo.get_file_content_on_view("filtered.txt", "dev")
            .unwrap()
            .unwrap(),
        b"one\ntwo\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("binary.dat", &outcome.snapshot_view)
            .unwrap()
            .unwrap(),
        final_binary
    );
}

#[test]
fn split_refusal_precedes_source_refs_and_filesystem_mutation() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("refuse.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("refuse.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    std::fs::write(&path, b"snapshot\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();
    let before_dev = direct_view_hashes(&repo, "dev");
    let snapshot_view = Repository::snapshot_view_name(working_copy);
    let before_snapshot = direct_view_hashes(&repo, &snapshot_view);
    let before_bytes = std::fs::read(&path).unwrap();

    let error = repo
        .split_snapshot(
            working_copy,
            IndexManifest::new(vec![
                IndexManifestEntry::present("refuse.txt", b"one\n".to_vec()),
                IndexManifestEntry::present("refuse.txt", b"two\n".to_vec()),
            ]),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SplitSnapshotError::Refused(SnapshotSplitRefusal::DuplicatePath { .. })
    ));
    assert_eq!(direct_view_hashes(&repo, "dev"), before_dev);
    assert_eq!(direct_view_hashes(&repo, &snapshot_view), before_snapshot);
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
}

#[test]
fn snapshot_retention_deletes_old_objects_through_verified_effects() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("retention.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("retention.txt", TrackingOptions::default())
        .unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();
    let mut snapshots = Vec::new();
    for value in [b"one\n".as_slice(), b"two\n", b"three\n"] {
        std::fs::write(&path, value).unwrap();
        snapshots.push(
            *repo
                .snapshot(
                    working_copy,
                    ChangeHeader::new("snapshot"),
                    record_options(),
                )
                .unwrap()
                .hash(),
        );
    }
    let status = repo.snapshot_status(working_copy).unwrap();
    assert_eq!(status.superseded_snapshots, 2);
    let retained = repo
        .prune_superseded_snapshots(working_copy, SnapshotRetentionPolicy { keep_superseded: 1 })
        .unwrap();
    assert_eq!(retained.retained, vec![snapshots[1]]);
    assert_eq!(retained.deleted, vec![snapshots[0]]);
    assert!(!repo.has_change(&snapshots[0]));
}

#[test]
fn promotion_rejects_working_copy_content_newer_than_snapshot() {
    let (directory, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let path = directory.path().join("stale.txt");
    std::fs::write(&path, b"baseline\n").unwrap();
    repo.add("stale.txt", TrackingOptions::default()).unwrap();
    repo.record(ChangeHeader::new("baseline"), record_options())
        .unwrap();

    std::fs::write(&path, b"snapshotted\n").unwrap();
    repo.snapshot(
        working_copy,
        ChangeHeader::new("snapshot"),
        record_options(),
    )
    .unwrap();
    std::fs::write(&path, b"newer unsnapshotted content\n").unwrap();

    let error = repo
        .promote_snapshot(
            working_copy,
            ChangeHeader::new("stale promotion"),
            record_options(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("create a new snapshot"));
}
