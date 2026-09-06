use super::*;
use crate::record::RecordOptions;
use crate::status::FileStatus;
use atomic_core::change::{GraphOp, InodeAttr, InodeKind};

fn record_all(repo: &TestRepository, message: &str) -> crate::record::RecordOutcome {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn chmod_records_graph_attribute_and_materializes_mode() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("script.sh");
    std::fs::write(&path, b"echo ok\n").unwrap();
    repo.add("script.sh", TrackingOptions::default()).unwrap();
    record_all(&repo, "add script");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::PermissionsChanged);

    let outcome = record_all(&repo, "chmod script");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Mode(0o755),
                ..
            } if path == "script.sh"
        )
    }));
    {
        use atomic_core::pristine::{InodeAttrTxnT, TreeTxnT, ViewTxnT};
        let txn = repo.pristine.read_txn().unwrap();
        let inode = txn.get_inode("script.sh").unwrap().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        let visibility = graph_visibility_closure(&txn, &view).unwrap();
        let visible = visibility.iter_dependency_first().copied().collect();
        assert_eq!(
            txn.resolve_inode_attr(position, atomic_core::change::InodeAttrName::Mode, &visible,)
                .unwrap()
                .value(),
            Some(InodeAttr::Mode(0o755))
        );
    }

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["script.sh".to_string()]))
        .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn concurrent_attribute_values_surface_as_status_conflict() {
    use atomic_core::pristine::{InodeAttrEvent, InodeAttrMutTxnT, MutTxnT, TreeTxnT, ViewTxnT};

    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("conflicted");
    std::fs::write(&path, b"content").unwrap();
    repo.add("conflicted", TrackingOptions::default()).unwrap();
    record_all(&repo, "add conflicted file");

    let mut txn = repo.pristine.write_txn().unwrap();
    let inode = txn.get_inode("conflicted").unwrap().unwrap();
    let position = txn.inode_position(inode).unwrap().unwrap();
    let first_hash = atomic_core::Hash::of(b"concurrent mode one");
    let second_hash = atomic_core::Hash::of(b"concurrent mode two");
    let first = txn.register_change(&first_hash).unwrap();
    let second = txn.register_change(&second_hash).unwrap();
    txn.put_change_deps(first, &[]).unwrap();
    txn.put_change_deps(second, &[]).unwrap();
    txn.put_inode_attr_event(
        inode,
        position,
        InodeAttrEvent::new(first, InodeAttr::Mode(0o700)).unwrap(),
    )
    .unwrap();
    txn.put_inode_attr_event(
        inode,
        position,
        InodeAttrEvent::new(second, InodeAttr::Mode(0o755)).unwrap(),
    )
    .unwrap();
    let mut view = txn.get_view("dev").unwrap().unwrap();
    txn.put_change(&mut view, first, &first_hash).unwrap();
    txn.put_change(&mut view, second, &second_hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();

    let status = repo.status(StatusOptions::default()).unwrap();
    let entry = status
        .entries()
        .iter()
        .find(|entry| entry.path() == std::path::Path::new("conflicted"))
        .unwrap();
    assert_eq!(entry.status(), FileStatus::Conflicted);
    assert_eq!(entry.details(), Some("inode attribute conflict"));
}

#[cfg(unix)]
#[test]
fn gitlink_lifecycle_switches_exact_payload_without_filter_corruption() {
    let (temp, mut repo) = create_temp_repo();
    let path = temp.path().join("dependency");
    let regular = b"regular baseline\n";
    let object_id = b"0123456789abcdef0123456789abcdef01234567";

    std::fs::write(&path, regular).unwrap();
    repo.add("dependency", TrackingOptions::default()).unwrap();
    record_all(&repo, "add regular dependency path");
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join(".git"), object_id).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);

    let outcome = record_all(&repo, "convert dependency to gitlink");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Gitlink),
                ..
            } if path == "dependency"
        )
    }));
    assert_eq!(
        repo.get_file_content_on_view("dependency", "feature")
            .unwrap(),
        Some(object_id.to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("dependency", "dev").unwrap(),
        Some(regular.to_vec())
    );

    repo.switch_view("dev").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), regular);
    assert_eq!(
        repo.get_file_content_on_view("dependency", "feature")
            .unwrap(),
        Some(object_id.to_vec())
    );

    repo.switch_view("feature").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_dir());
    assert_eq!(std::fs::read(path.join(".git")).unwrap(), object_id);
    assert!(repo.status(StatusOptions::default()).unwrap().is_clean());

    repo.switch_view("dev").unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), regular);
}

#[cfg(unix)]
#[test]
fn regular_to_dangling_symlink_records_type_and_materializes_target() {
    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("link");
    std::fs::write(&path, b"regular").unwrap();
    repo.add("link", TrackingOptions::default()).unwrap();
    record_all(&repo, "add regular");

    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing-target", &path).unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);

    let outcome = record_all(&repo, "make symlink");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Symlink),
                ..
            } if path == "link"
        )
    }));

    std::fs::remove_file(&path).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["link".to_string()]))
        .unwrap();
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(&path).unwrap(),
        std::path::Path::new("missing-target")
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"regular again").unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert_eq!(status.entries()[0].status(), FileStatus::TypeChanged);
    let outcome = record_all(&repo, "make regular");
    assert!(outcome.change().hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SetAttr {
                path,
                value: InodeAttr::Kind(InodeKind::Regular),
                ..
            } if path == "link"
        )
    }));
    std::fs::remove_file(&path).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(["link".to_string()]))
        .unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(std::fs::read(&path).unwrap(), b"regular again");
}
