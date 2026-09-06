# Atomic Git Causal Bridge — Implementation Tracker

Source: [`RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md`](RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md)

Last audited: 2026-09-05

This file is the operational backlog for the RFC. The RFC remains the normative
architecture and acceptance contract; this tracker answers what is done, what is
ready, what is blocked, and which bounded work unit should become an Atomic intent
next.

## Status legend

- `[x] DONE` — acceptance is covered by a completed Atomic intent and evidence.
- `[ ] READY` — every prerequisite is complete; safe to plan/start.
- `[ ] BLOCKED` — do not start until every listed prerequisite is done.
- `[ ] FIXTURE` — executable evidence for a future TODO, not implementation completion.
- `Intent: pending` — allocate with `atomic intent new` before implementation.

## Operating rules

1. Every implementation TODO below becomes one directive-based Atomic intent before code changes begin.
2. Add the resulting human key in the `Intent` column and encode prerequisites in the intent's `blocked_by` frontmatter.
3. Do not mark a TODO done from code inspection alone: its intent must be `done`, conforming, attested, verified, and linked to test evidence.
4. Keep blocked work in `backlog`. Use `planned` for the single primary next item.
5. Phase 0 may proceed in parallel with Phase N. Phase 1 does not start until Phase 0 and the remaining Phase N integrity prerequisites are complete.
6. Experimental bridge MVP code is reusable evidence, not final phase completion unless the final RFC acceptance is met.

## Ready queue

### Primary next

- [ ] **READY — CB-2B: Graph-safe staged/remainder snapshot split, retention, and snapshot UX**
  Intent: pending · Priority: high · Prerequisites: CB-2A, CB-3A, and CB-3C (done)
  Split snapshots into independently materializable staged and remainder changes, add structured inseparability refusal, retention, status, and `diff --snapshot`.

### Recently completed critical path

- [x] **DONE — CB-2A:** `ATOM::continuouslee::82` / `01M1W0QE1PVXN2PQC03AF25A69`
- [x] **DONE — CB-3A:** `ATOM::continuouslee::83` / `01M1W0QG4GVT4ZNJ37PB661P7G`
- [x] **DONE — CB-3B:** `ATOM::continuouslee::84` / `01M1W7NBBVSRG97GY0Y6AVYD8C`
- [x] **DONE — CB-3C:** `ATOM::continuouslee::85` / `01M1W7NBQA8CJRMQAC84T58X6G`

Phase 1, the shared CB-FMT1 format foundation, and the Phase 2/3 prerequisites for CB-2B are complete.

---

## Completed foundations and executable evidence

| Status | Work | Atomic intent | Notes |
|---|---|---|---|
| [x] DONE | RFC architecture and revision audit | `ATOM::continuouslee::52`, `ATOM::continuouslee::53`, `ATOM::continuouslee::54` | Normative design; not production phase completion. |
| [x] DONE | Clean bidirectional bridge MVP | `ATOM::continuouslee::56` / `01M1HRR0W212QBEMNBV041SBQN` | Regular UTF-8 attached-branch prototype; attestation currently needs refresh. |
| [x] DONE | Foreground bridge switch and raw Git switch adoption | `ATOM::continuouslee::58` / `01M1HSSHDW3FED05GDR67DWTMT` | Prototype evidence for later Phases 7–8. |
| [x] DONE | Pre-write collision/failure safety | `ATOM::continuouslee::59` / `01M1HTZW5RTYVG5005S2JN51R8` | Does not prove mid-write recovery. |
| [x] DONE | Mid-materialization effect→receipt recovery contract | `ATOM::continuouslee::60` / `01M1HVG6H346QG22R3C08486S1` | Original expected-red assertions are promoted unchanged as numbered harness 43; with native-index repair checks it passes 21/21 under CB-1B. |
| [x] DONE | N5 typed absent/present materialization | `ATOM::continuouslee::61` / `01M1HXJDQRNSXXNK66290EYR1B` | Covers zero-byte files and all output modes. |
| [x] DONE | N7 canonical graph visibility closure | `ATOM::continuouslee::62` / `01M1J49E5GE48SZNP0249BPY46` | Membership and dependency-expanded visibility are typed separately. |
| [x] DONE | Fail-closed graph iterator/retrieval errors | `ATOM::continuouslee::63` / `01M1KJP3MTRNKNEXMTB8BG88DX` | Supporting integrity prerequisite. |
| [x] DONE | Fail-closed semantic render errors | `ATOM::continuouslee::64` / `01M1KRAZ4BJN9QABKW8BMR70DZ` | Supporting integrity prerequisite. |
| [x] DONE | N1 nested parent globalization | `ATOM::continuouslee::65` / `01M1KTZ8WWP5CF17VSP7ARFPZ3` | Parent-first anchors and parent dependencies. |
| [x] DONE | Strict workspace cleanup gate | `ATOM::continuouslee::66` / `01M1KXEV9N2QB10FQRXX9TDK53` | Format, strict clippy, workspace tests, delete harness. |
| [x] DONE | N34 directory delete, identity-preserving undelete, and empty-directory lifecycle | `ATOM::continuouslee::70` / `01M1M5AYVPFGP7G1Y4KQRRKM9N` | Exact graph claims, causal aliveness, all materialization modes, reopen and sibling-view evidence. |
| [x] DONE | 0B local WIP recovery and durable incomplete managed-agent outcome | `ATOM::continuouslee::71` / `01M1M5B12HVZ3C5DDDA1ZWGNNQ` | Alternate-index repository bytes, create-only reflogged refs, no false recording, local-only publication. |
| [x] DONE | 0D append-only advisory Git event journal | `ATOM::continuouslee::72` / `01M1M5B5HQWR13R1508XT2RYJG` | Common-dir/custom-hook-safe dispatcher, deferred read-only receipts, hook-independent correctness. |
| [x] DONE | N6 durable causal path claims and graph-backed name resolution | `ATOM::continuouslee::73` / `01M1MA1ZZ6ATGBE5EEK3HGQMW0` | Strict TREE bijection, A11/A12 order independence, reversible solve visibility, and lossless legacy migration. |
| [x] DONE | 0C shared stale-baseline guard | `ATOM::continuouslee::74` / `01M1MA2019Z6XPBY3Z0TRJNYJ4` | One early guard across readers/writers/materializers/agents with v2 checkpoint evidence and WIP-backed refusal. |
| [x] DONE | N8 native rename-plus-edit identity and explicit move evidence | `ATOM::continuouslee::75` / `01M1PBEFQ779K72YF5YAW6RS9W` | Authoritative staged moves retain inode/trunk through arbitrary edits; similarity is `ProbableMove`; ambiguous candidates remain delete+add with `RenameUnresolved`. |
| [x] DONE | N9 native derived-index verification and atomic repair | `ATOM::continuouslee::76` / `01M1PH46D0HDWEJWNZYAS2EEY7` | Cache-independent all-view oracle, deterministic diagnostics, ambiguity/staging preservation, immediate all-or-nothing replacement, and read-only doctor checks. |
| [x] DONE | 1A persistent working-copy identity and API boundary | `ATOM::continuouslee::77` / `01M1PR1V7M6R5V2AYED4WAVS13` | Versioned pristine records, safe legacy/copy migration, distinct linked worktrees and sandboxes, explicit working-copy capabilities, scoped caches/shelves, and derived `current_view`. |
| [x] DONE | 1B durable operation/effect journal, ordered locks, and crash recovery | `ATOM::continuouslee::79` / `01M1QA9RZQ5RA0HG3HDST78R1J` | Canonical append-only operation/receipt storage, CAS heads, common→working-copy→pristine→shelf locking, per-effect leases, inverse/startup recovery, canonical imported deletes, and harnesses 05/43. |
| [x] DONE | 1C operation commands, inverse deltas, head consolidation, and native routing | `ATOM::continuouslee::80` / `01M1SFDT134Z6N3S6VKH1S59FR` | V1-preserving operation V2, `op log|show|undo|restore`, shift-aware historical restore, shared repository heads, deterministic consolidation/`Diverged`, routed native mutations, and harness 44 (31/31). |
| [x] DONE | FMT1 change-object lifecycle/origin/frontier format | `ATOM::continuouslee::81` / `01M1TG0HTNBC6MX7563WA7BN2C` | ATOM schema V2 hashed envelope, immutable V1 byte/hash fixture, lossless `extra_known`/metadata, checked lifecycle/origin combinations, verified causal frontier closure in conflict checks, and pre-write repository capability fence. |

---

## Phase N — Native tree and materialization integrity

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-N1 | Nested parent globalization | — | `ATOM::continuouslee::65` |
| [x] DONE | CB-N2 | Central operation-aware tree projection and directory occupancy | N1, N5, N7 | `ATOM::continuouslee::68` |
| [x] DONE | CB-N34 | Complete `DirDel`, file/directory undelete, and explicit empty-directory lifecycle | N2 | `ATOM::continuouslee::70` / `01M1M5AYVPFGP7G1Y4KQRRKM9N` |
| [x] DONE | CB-N5 | Typed materialization/content presence | — | `ATOM::continuouslee::61` |
| [x] DONE | CB-N6 | Durable `PATH_CLAIMS` and graph-backed name-conflict resolution | N2, N34 | `ATOM::continuouslee::73` / `01M1MA1ZZ6ATGBE5EEK3HGQMW0` |
| [x] DONE | CB-N7 | Canonical membership/visibility closure | — | `ATOM::continuouslee::62` |
| [x] DONE | CB-N8 | Native rename-plus-edit with stable inode and `ProbableMove` evidence | N2, N6 | `ATOM::continuouslee::75` / `01M1PBEFQ779K72YF5YAW6RS9W` |
| [x] DONE | CB-N9 | Verify and repair native derived tree indexes | N2, N34, N6, N8 | `ATOM::continuouslee::76` / `01M1PH46D0HDWEJWNZYAS2EEY7` |

### CB-N34 definition of done

`DirDel` deletes the actual parent→name→inode structural claims created by
`DirAdd`; normal record/globalize constructors emit `FileUndel` and `DirUndel`;
undelete preserves inode identity; explicit empty directories materialize through
full and prefix output; occupancy transitions remain correct after reopen and
across sibling views.

### CB-N6 definition of done

`PATH_CLAIMS` retains every visible claimant, `TREE`↔`REV_TREE` remains bijective,
`SolveNameConflict` is durably recorded/applied, and losing claims remain
recoverable where the resolution is not visible. No last-writer/view-order winner.

### CB-N8 definition of done

Pure rename, rename plus small/large edits, cross-directory move, and move into a
new directory preserve the inode when identity evidence is authoritative. Heuristic
matches are explicitly `ProbableMove`; ambiguous cases remain delete+add with loss
evidence.

### CB-N9 definition of done

`atomic doctor` detects injected stale/missing rows across `TREE`, `REV_TREE`,
`PATH_CLAIMS`, `INODES`, `REV_INODES`, `DIRECTORIES`, `DIR_EMPTY`, and conflict
projections. Safe repair deterministically rebuilds derived caches without mutating
graph facts or deleting ambiguous content.

---

## Phase 0 — Safety guard, drift diagnostics, and WIP preservation

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-0A | Read-only Git observer, provisional checkpoint/`Unanchored`, `status --no-reconcile` | — | `ATOM::continuouslee::69` |
| [x] DONE | CB-0B | Local WIP refs and durable incomplete-agent outcome | 0A | `ATOM::continuouslee::71` / `01M1M5B12HVZ3C5DDDA1ZWGNNQ` |
| [x] DONE | CB-0C | Shared stale-baseline guard across all working-copy commands | 0A, 0B | `ATOM::continuouslee::74` / `01M1MA2019Z6XPBY3Z0TRJNYJ4` |
| [x] DONE | CB-0D | Append-only Git event journal and advisory `post-checkout` | 0A | `ATOM::continuouslee::72` / `01M1M5B5HQWR13R1508XT2RYJG` |

### CB-0B definition of done

Before refusing drifted tracked work, repository bytes are preserved under a
create-only/reflogged `refs/atomic/wip/...`; managed turn-end records an incomplete
session with reason, paths, and recovery ref and returns non-zero. WIP refs never
push and never invent pre-checkout provenance.

### CB-0C definition of done

Status, diff, record, add, materialize, view switch, and agent turn-end invoke one
guard before interpreting filesystem state. Drift reports old/current Atomic and
Git states, refs, manifest roots, unsafe operation, and exact remediation. No-Git
repositories are unchanged.

### CB-0D definition of done

Bridge enablement installs a composable common-dir/`core.hooksPath`-aware advisory
dispatcher. `post-checkout` appends immutable evidence and schedules deferred
observation; existing hooks are preserved; correctness remains independent of hook
execution.

---

## Phase 1 — Persistent working copies and operation/effect journal

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-1A | Persistent working-copy identity and API boundary | Phase N, 0C | `ATOM::continuouslee::77` / `01M1PR1V7M6R5V2AYED4WAVS13` |
| [x] DONE | CB-1B | Durable operation/effect journal, ordered locks, crash recovery | 1A, N2 | `ATOM::continuouslee::79` / `01M1QA9RZQ5RA0HG3HDST78R1J` |
| [x] DONE | CB-1C | `atomic op` commands, inverse deltas, operation heads, native command routing | 1B | `ATOM::continuouslee::80` / `01M1SFDT134Z6N3S6VKH1S59FR` |

### CB-1A definition of done

Stable `WorkingCopyId` records survive reopen; linked Git worktrees receive distinct
records; copied IDs and legacy empty identity files migrate safely; every
working-copy-aware repository API requires an ID; `.atomic/current_view` becomes a
derived compatibility artifact.

### CB-1B definition of done

Versioned `OPERATIONS`, `OP_HEADS`, and `EFFECT_RECEIPTS` use canonical
self-reference-free hashes. Common→working-copy→pristine→shelf locking is enforced.
Every external effect has expected-old/new leases and immutable receipts; all crash
points recover idempotently and the existing red recovery fixture turns green.

### CB-1C definition of done

`atomic op log|show|undo|restore` works; switch and record undo preserve content;
commuting operation heads consolidate and incompatible heads become explicit
`Diverged`; record/insert/unrecord/tag/pull/push/materialize all emit operations.
Evidence: V1/V2 codec and 42 focused repository operation tests, real CLI integration,
linked-worktree shared-head and historical restore coverage, workspace tests 138/138,
recovery harness 43 at 21/21, and operation harness 44 at 31/31.

---

## Shared Phase 2/3 format foundation

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-FMT1 | Change-object format vNext for snapshot lifecycle and Git origin/frontier | 1A, 1C | `ATOM::continuouslee::81` / `01M1TG0HTNBC6MX7563WA7BN2C` |

### CB-FMT1 definition of done

New objects losslessly encode `ChangeKind`, snapshot owner, `supersedes`,
`ChangeOrigin`, `CausalFrontier`, and `extra_known`; legacy fixture bytes retain
their hashes without re-encoding; impossible combinations fail; old clients fail
closed on the repository capability fence. Git parents never enter Atomic
`dependencies`.

Evidence: ATOM schema V2 frozen-envelope round trips and invalid-combination fixtures;
immutable V1 fixture object hash `KQEBIVO7FVXRZ5PWLT75G267BRXZU5GKHWLV62MHBQDG67VE5BGQ`;
complete frontier-index verification wired into zombie checks; eight repository
capability/open/write tests; full `atomic-core`; `atomic-repository` excluding four
pre-existing long-running property/import tests that exceeded 10- and 20-minute
bounds; and strict workspace clippy with `-D warnings`.

---

## Phase 2 — Snapshot changes, promotion, and split

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-2A | Complete baseline-relative snapshot lifecycle and promotion | FMT1, 1C | `ATOM::continuouslee::82` / `01M1W0QE1PVXN2PQC03AF25A69` |
| [ ] READY | CB-2B | Graph-safe staged/remainder snapshot split, retention, and snapshot UX | 2A, 3A, 3C | pending |

### CB-2A definition of done

Each working copy owns `wc/<id>`; repeated snapshots form a supersession chain with
only the head visible; each head materializes independently from the durable
baseline; promotion emits identical durable content with no snapshot dependency;
snapshots cannot enter Shared views, normal logs, or push.

### CB-2B definition of done

`split_snapshot(index_manifest)` independently reassembles baseline→index durable
state and durable→worktree remainder. Hunk subsets, adjacent edits, delete+insert,
rename+edit, mode changes, and binary fallback materialize identically; inseparable
operations return structured refusal; retention, status, and `diff --snapshot` work.

---

## Phase 3 — Tree-semantic completeness

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [x] DONE | CB-3A | Causal inode attributes and native mode/kind lifecycle | FMT1, N34 | `ATOM::continuouslee::83` / `01M1W0QG4GVT4ZNJ37PB661P7G` |
| [x] DONE | CB-3B | Effective projection closure, SetId index, and topological-order proof | 3A, N6, N7 | `ATOM::continuouslee::84` / `01M1W7NBBVSRG97GY0Y6AVYD8C` |
| [x] DONE | CB-3C | Repository-byte filters, opaque tracked content, and empty-directory loss notes | FMT1, 3A, N34 | `ATOM::continuouslee::85` / `01M1W7NBQA8CJRMQAC84T58X6G` |

### CB-3A definition of done

`SetAttr` is an additive causal multi-value register across serialization,
globalization, application, retrieval, both graph indexes, semantic ops, and
conflicts. Chmod, file↔symlink, dangling links, and gitlinks record/materialize
across views and report graph-backed `P`/`T`.

### CB-3B definition of done

One effective projection closure feeds projection and SetId. SetId v1 bytes remain
unchanged and use a separate versioned index. `atomic view show` prints Merkle and
SetId. Property tests prove equal content, attributes, semantic state, and persisted
conflicts for every valid topological order.

### CB-3C definition of done

`ContentFilter` supports eol/text/ident, LFS-pointer passthrough, and bounded
external filters; required-filter failures block adoption. Large Git-tracked binary
content records as opaque graph content with warning. Empty directories project
away with explicit `LossNote::EmptyDirectory`.

---

## Phase 4 — Manifest and equivalence engine

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-4A | Canonical manifest model and Atomic/Git tree builders | 2B, 3B, 3C, N9 | pending |
| [ ] BLOCKED | CB-4B | Stage/worktree observation and complete equivalence integration | 4A | pending |
| [ ] BLOCKED | CB-4C | `FILE_INDEX_V2`, change-source tiers, and performance gates | 4A, 4B | pending |

### CB-4A definition of done

Versioned raw-byte `RepoPath`, repository manifest, Git index state, worktree
observation, and conversion policy have deterministic roots. Graph-derived
`ProjectTree` and Git-tree builders agree on bytes, modes, kinds, links, gitlinks,
empty files, exclusions, and SHA-1/SHA-256 object formats.

### CB-4B definition of done

Stage-aware index and worktree builders cover intent-to-add, flags, sparse entries,
conflict stages, filters, modes, and platform capabilities. Structured mismatch
reports detect stale files, modes, links, newlines, empties, case collisions, and
forged headers. Push/import use full equivalence.

### CB-4C definition of done

`FILE_INDEX_V2` includes racy-stat metadata. Scan, fsmonitor, and Watchman candidate
sources reverify to identical roots; errors/overflow/unknown tokens degrade to scan;
100k-file warm performance targets pass.

---

## Phase 5 — Shared workspace transaction

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-5A | Shared workspace transaction core and remediation objects | 1C, 4B, 4C, 0C | pending |
| [ ] BLOCKED | CB-5B | Route local repository commands and retire direct view reads | 5A | pending |
| [ ] BLOCKED | CB-5C | Route Git/network/agent boundaries and journal bridge writes | 5A, 5B, 2A | pending |

### CB-5A definition of done

`begin_workspace_txn(Reconcile|Observe|Force)` implements lock ordering,
checkpoint/Git observation, sequence-operation detection, HEAD-before-filesystem
ordering, bounded plans, and TOCTOU retry. Observe never mutates. Merge/rebase and
unanchored states return typed remediation objects.

### CB-5B definition of done

Status, diff, record, add/rm, view switch/create/publish, materialize, insert,
unrecord, reinsert, revise, and tag require an explicit transaction mode and derive
view identity from the working-copy record. A coverage test detects bypasses.

### CB-5C definition of done

Pull, push, clone, Git import/export, and agent boundaries use workspace
transactions; sequence operations yield the specified observe/refuse/snapshot
behavior; Watchman is bracketed with `atomic-bridge`; every bridge Git write is
journaled with its operation ID before visibility.

---

## Phase 6 — Bindings, resurrection, trust, and privacy

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-6A | Signed binding object, trust policy, and privacy format | 5C, 4A | pending |
| [ ] BLOCKED | CB-6B | Binding closure transport and bounded pack fallback | 6A | pending |
| [ ] BLOCKED | CB-6C | Exact resurrection and checked-cache cutover | 6B, 4B, 5A | pending |

### CB-6A definition of done

Canonical `GitStateBinding` supports SHA-1/SHA-256, immutable storage/create-only
refs, Ed25519 signing, trust policy, and summary-only Git metadata. Any field tamper
fails; unknown signers retain recomputed content marked untrusted; packed trees
contain no prompts, transcripts, or unhashed bytes.

### CB-6B definition of done

Closure fetch prefers Atomic remote and falls back to bounded verified binding packs.
Binding refs are create-only; malformed, cyclic, oversized, or traversal packs fail
closed; WIP refs never transfer.

### CB-6C definition of done

Fresh clones restore exact hashes, dependencies, semantic identities, conflicts,
provenance roots, and Merkle order from a binding; squash restores originals;
projection comparison is mandatory; `GIT_SHA_INDEX` becomes a recomputation-checked
cache only.

---

## Phase 7 — Anchoring and Git HEAD adoption

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-7A | Enable bridge, create Anchor, and adopt bound Git HEADs | Phase N, Phase 0–6 | pending |
| [ ] BLOCKED | CB-7B | Snapshot-safe, shelf-safe, interruption-safe HEAD adoption | 7A, 2A, 5A, 1B | pending |

### CB-7A definition of done

Equivalent repository/index/worktree layers produce a signed Anchor binding. Bound
branch, detached, historical, reset, renamed/deleted, and unborn cases adopt or
return the specified typed result, with no rewrite when manifests already match.
Unbound Git adoption remains disabled until Phase 9.

### CB-7B definition of done

Pre-captured edits are reassembled against the new baseline; absent capture becomes
an unknown-origin snapshot; shelves swap collision-safely; interruption resumes or
rolls back under immutable receipts without inventing old-baseline provenance.

---

## Phase 8 — Atomic-origin projection and HEAD policy

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-8A | Canonical Atomic→Git tree/commit/index/tag and view-scope HEAD policy | 4A, 6A, N6 | pending |
| [ ] BLOCKED | CB-8B | Conflict projection and leased Git-effect recovery | 8A, 1B, 6A | pending |

### CB-8A definition of done

Graph-derived `ProjectTree` and operation-specific commits leave both statuses clean.
Shared views update branches; Drafts remain detached with
`refs/atomic/views/<name>` reachability; snapshot bytes remain worktree-only; tags
round-trip; signed raw commits are preserved.

### CB-8B definition of done

Draft conflict commits plus complete packs restore the same conflict set; Shared
export refuses without explicit allowance; every ref/index/materialization crash
point recovers under expected-old leases; the existing red recovery fixture passes
unchanged.

---

## Phase 9 — Foreign Git synthesis

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-9A | Bridge-transaction import through normal assembly with hashed Git origin | 3A, 4A, 5A, N2 | pending |
| [ ] BLOCKED | CB-9B | Causal merge, empty-commit, squash, and rewrite synthesis | 9A, FMT1 | pending |
| [ ] BLOCKED | CB-9C | Tree-semantic preservation and per-state synthesis verification | 9A, 3C, 4B, N8 | pending |

### CB-9A definition of done

Root and single-parent commits use normal assembly/globalization inside workspace
transactions; ordered Git parents and derivation are hashed; Git parents never
become Atomic dependencies; old imported bytes remain authoritative.

### CB-9B definition of done

Merge trees derive from union state plus `GitResolution` covering every parent
frontier; distinct empty commits stay distinct; squash/rewrite candidates enter
review; octopus and serialize/reload conflict tests pass.

### CB-9C definition of done

Rename, mode, symlink, gitlink, raw-path, CRLF, binary, and filter corpus projects
identically at every commit; supported imports emit `FileOps`; uncertain renames
carry explicit loss evidence; verification never uses the active worktree.

---

## Phase 10 — Ref reconciliation and transport

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-10A | Persist and reconcile local ref/view mappings | 6C, 8A, 9B, 9C | pending |
| [ ] BLOCKED | CB-10B | Transport bindings and bootstrap Git clones | 10A, 6B, 9C | pending |

### CB-10A definition of done

Git-only movement imports, Atomic-only movement exports, and incompatible movement
persists `Diverged` without moving either side. Publish/rename/delete follows view
scope and every local ref movement uses expected-old checks.

### CB-10B definition of done

Binding refs/packs transfer with CAS and retry; remote lease failure refuses push;
fetch refspecs/namespace fallback are explicit; Git clone plus
`atomic init --adopt-git` restores exact bound closure.

---

## Phase 11 — Status, diff, staging, and tracking parity

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-11A | Five-layer staging model and versioned Git status/diff parity | 4B, 5A | pending |

### CB-11A definition of done

Every RFC §9.2 row and listed staging/tracking edge case passes. `StagingState`,
stage/unstage, colocated add/reset semantics, `status/diff --git`, Git-authoritative
ignore mirroring, machine-readable bridge state/origin, and golden Git comparisons
are implemented without mutating durable tracking from index-only operations.

---

## Phase 12 — Managed-agent guarantees

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-12A | Exact-or-incomplete managed Git turns and signed boundaries | 2B, 5C, 6C, 9C, 11A | pending |
| [ ] BLOCKED | CB-12B | Protected publication provenance enforcement | 12A, 8A, 10B | pending |

### CB-12A definition of done

Authenticated partial Git commits split into durable change plus pending remainder;
Git-only operations produce `RepositoryOperations`; hook bypass/plumbing/alternate
index becomes synthesized and durably incomplete; signed attestations cover exact
boundaries, changes, bindings, operations, and provenance. False `EmptyTurn` is
removed and `atomic agent repair` exists.

### CB-12B definition of done

Shared insert/promotion, Atomic push, Git export, and trusted server/CI verification
refuse incomplete, unexplained, or untrusted managed-session work. Local hooks remain
advisory; unenforced direct Git push warns rather than claiming protection.

---

## Phase 13 — Recovery, migration, rollout, and optional watcher

| Status | ID | Work unit | Prerequisites | Intent |
|---|---|---|---|---|
| [ ] BLOCKED | CB-13A | Unified bridge retention, recovery, doctor, and crash matrix | Phases 1–12 | pending |
| [ ] BLOCKED | CB-13B | Transactional Shadow migration and repository capability fence | 13A, final schemas | pending |
| [ ] BLOCKED | CB-13C | Observability, operating/security guides, and rollout gates | 13B, 11A, 12B | pending |
| [ ] BLOCKED | CB-13D | Optional metadata-only bridge watcher | 4C, 5A, 10A, 13A | pending |

### CB-13A definition of done

Content and audit retention roots cover operations, receipts, working copies,
conflicts, advertised bindings, incomplete sessions, snapshots/WIP, and keep refs.
Doctor reports every RFC fault class; repair acts only under leases; crash and
retention matrices pass; age never deletes the only unbound copy.

### CB-13B definition of done

Old change bytes/hashes remain authoritative; legacy indexes/hooks/trailers become
verified candidate bindings or review items; old clients fail closed; locked
transactional cutover removes legacy writers and supports rollback. Shadow and the
colocated bridge cannot both be active.

### CB-13C definition of done

Reconciliation, drift, synthesis, loss, gate refusal, and watcher degradation metrics
are emitted. User, agent, migration, and privacy/security guides exist. Measured
criteria gate opt-in and default enablement; no document claims watchers/hooks are a
correctness boundary.

### CB-13D definition of done

Watcher-off, fsmonitor, and Watchman runs reach identical logical states. The daemon
never materializes or moves refs, does not reconcile during locks/sequence
operations, suppresses self-events, emits managed-session notices, and can be killed
without changing the next command outcome.

---

## Dependency waves

1. **Now:** CB-2B; CB-2A and CB-3A/3B/3C are complete.
2. **Native integrity:** Phase N is complete through CB-N9.
3. **Phase 0 completion:** CB-0A, CB-0B, CB-0C, and CB-0D are done.
4. **Operation and format substrate:** CB-1A → CB-1B → CB-1C → CB-FMT1 are done.
5. **Snapshots and semantics:** CB-2A; CB-3A → CB-3B/CB-3C → CB-2B.
6. **Equivalence:** CB-4A → CB-4B → CB-4C.
7. **Shared transaction:** CB-5A → CB-5B → CB-5C.
8. **Bindings:** CB-6A → CB-6B → CB-6C.
9. **Projection/adoption/synthesis:** CB-7A/CB-8A/CB-9A; then their dependent units.
10. **Refs and staging:** CB-10A → CB-10B; CB-11A after CB-4B/CB-5A.
11. **Agent trust:** CB-12A → CB-12B.
12. **Hardening/rollout:** CB-13A → CB-13B → CB-13C; CB-13D remains optional.

## Administrative follow-up

- Refresh the stale attestation for `ATOM::continuouslee::56` after confirming its current directives still match the MVP evidence.
- Allocate the CB-2B intent next with CB-2A, CB-3A, and CB-3C encoded in `blocked_by` frontmatter; replace `pending` with its human key and UID.
