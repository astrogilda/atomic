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
