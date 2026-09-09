use atomic_core::change::session::SessionTurn;
use atomic_core::change::ProvenanceGraph;
use atomic_core::types::Hash;
use atomic_repository::redb_change_store::{
    ProvenanceCheckpointPhase, ProvenanceCheckpointSource, ProvenanceTurnState, RedbChangeStore,
    RedbStoreError,
};
use atomic_repository::{ChangeStore, Repository, DEFAULT_CACHE_CAPACITY};

#[test]
fn checkpoint_recovers_idempotently_across_every_publication_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let repository = Repository::init(&root).unwrap();
    let redb_path = repository.redb_change_store_path();
    let changes_dir = repository.changes_dir();
    drop(repository);

    let store = RedbChangeStore::open(&redb_path).unwrap();
    let running = store
        .reserve_provenance_turn("checkpoint-session", 1, 10)
        .unwrap();
    store
        .append_provenance_envelope(
            running.provenance_id,
            running.generation,
            "goal",
            br#"{"schema_version":1,"event_id":"goal"}"#,
            11,
        )
        .unwrap();
    store
        .append_provenance_envelope(
            running.provenance_id,
            running.generation,
            "terminal",
            br#"{"schema_version":1,"event_id":"terminal"}"#,
            12,
        )
        .unwrap();

    let source_change = Hash::of(b"recorded source change");
    let source = ProvenanceCheckpointSource {
        agent_name: "opencode".to_string(),
        agent_display_name: "OpenCode".to_string(),
        agent_vendor: "openai".to_string(),
        change_hashes: vec![source_change],
        previous_provenance: None,
        plan_id: Some("ATOM-108".to_string()),
        ledger_turn_number: 0,
    };
    let prepared = store
        .prepare_provenance_checkpoint(
            running.provenance_id,
            running.generation,
            source.clone(),
            13,
        )
        .unwrap();
    assert_eq!(prepared.phase, ProvenanceCheckpointPhase::Prepared);
    assert_eq!(prepared.frozen_event_count, 2);
    assert!(matches!(
        store.append_provenance_envelope(
            running.provenance_id,
            running.generation,
            "late",
            b"late",
            14,
        ),
        Err(RedbStoreError::ProvenanceFenced { .. })
    ));
    drop(store);

    // Crash after prepare: the attempt and exact exclusive cutoff survive.
    let store = RedbChangeStore::open(&redb_path).unwrap();
    let prepared_retry = store
        .prepare_provenance_checkpoint(
            running.provenance_id,
            running.generation,
            source.clone(),
            15,
        )
        .unwrap();
    assert_eq!(prepared_retry, prepared);
    let frozen = store
        .load_frozen_provenance_envelopes(running.provenance_id)
        .unwrap();
    assert_eq!(frozen.len(), 2);
    assert_eq!(
        frozen.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );

    let graph = ProvenanceGraph::builder("checkpoint-session", "opencode")
        .agent_display_name("OpenCode")
        .agent_vendor("openai")
        .changes_explained(vec![source_change])
        .timestamp(12_000)
        .plan_id("ATOM-108")
        .build();
    let change_store = ChangeStore::new(changes_dir, DEFAULT_CACHE_CAPACITY).unwrap();
    let provenance_hash = change_store.save_provenance_graph(&graph).unwrap();
    let turn = SessionTurn {
        session_id: "checkpoint-session".to_string(),
        turn_number: 0,
        goal: None,
        provenance_hash,
        change_hashes: vec![source_change],
        previous_provenance: None,
        timestamp: graph.timestamp,
        plan_id: Some("ATOM-108".to_string()),
        todos: Vec::new(),
    };
    let bound = store
        .bind_provenance_checkpoint_hash(
            running.provenance_id,
            prepared.attempt_generation,
            provenance_hash,
            turn.clone(),
            16,
        )
        .unwrap();
    assert_eq!(bound.phase, ProvenanceCheckpointPhase::HashBound);
    drop(store);

    // Crash after hash bind: the exact hash and SessionTurn survive and rebind idempotently.
    let store = RedbChangeStore::open(&redb_path).unwrap();
    let rebound = store
        .bind_provenance_checkpoint_hash(
            running.provenance_id,
            prepared.attempt_generation,
            provenance_hash,
            turn.clone(),
            17,
        )
        .unwrap();
    assert_eq!(rebound, bound);

    let repository = Repository::open(&root).unwrap();
    let publication = repository
        .publish_provenance_checkpoint(&graph, turn.clone())
        .unwrap();
    let retry_publication = repository
        .publish_provenance_checkpoint(&graph, turn.clone())
        .unwrap();
    assert_eq!(retry_publication, publication);
    let (_, turns) = repository
        .get_session_ledger("checkpoint-session")
        .unwrap()
        .unwrap();
    assert_eq!(turns, vec![turn.clone()]);
    assert_eq!(
        repository.get_session_head("checkpoint-session").unwrap(),
        Some(publication.manifest_hash)
    );

    // A different immutable turn at the same ordinal cannot rewrite the completed row.
    let conflicting_graph = ProvenanceGraph::builder("checkpoint-session", "opencode")
        .agent_display_name("OpenCode")
        .agent_vendor("openai")
        .changes_explained(vec![source_change])
        .timestamp(13_000)
        .plan_id("ATOM-108")
        .build();
    let conflicting_hash = change_store
        .save_provenance_graph(&conflicting_graph)
        .unwrap();
    let mut conflicting_turn = turn.clone();
    conflicting_turn.provenance_hash = conflicting_hash;
    conflicting_turn.timestamp = conflicting_graph.timestamp;
    assert!(repository
        .publish_provenance_checkpoint(&conflicting_graph, conflicting_turn)
        .is_err());
    let (_, unchanged) = repository
        .get_session_ledger("checkpoint-session")
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, vec![turn]);
    drop(repository);

    // Crash after pristine publication: retrying publication above is harmless,
    // and acknowledgement is itself idempotent after completion.
    let completed = store
        .acknowledge_provenance_checkpoint(
            running.provenance_id,
            prepared.attempt_generation,
            publication.manifest_hash,
            18,
        )
        .unwrap();
    assert!(matches!(completed.state, ProvenanceTurnState::Completed));
    assert_eq!(completed.final_hash, Some(provenance_hash));
    drop(store);

    let reopened = RedbChangeStore::open(&redb_path).unwrap();
    let acknowledged_retry = reopened
        .acknowledge_provenance_checkpoint(
            running.provenance_id,
            prepared.attempt_generation,
            publication.manifest_hash,
            19,
        )
        .unwrap();
    assert_eq!(acknowledged_retry, completed);
    let persisted = reopened
        .get_provenance_turn(running.provenance_id)
        .unwrap()
        .unwrap();
    assert_eq!(persisted, completed);
    assert_eq!(
        persisted.checkpoint_attempt.unwrap().phase,
        ProvenanceCheckpointPhase::Published
    );
}
