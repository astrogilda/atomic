# Atomic delivery checkpoint — October 2, 2026

This entrypoint covers the Atomic contribution lane. It does not replace the
separately owned full Probity program, its allocation, targets or release work.
Read [HANDOFF.md](HANDOFF.md), [actions.json](actions.json) and
[DELIVERY-STATE.json](DELIVERY-STATE.json) before resuming.

The earlier adoption assessment was preparation. Native Rust execution, an
installed consumer and merged support records have since been delivered.

## Delivered support work

- [Observer #52](https://github.com/probityai/agent-evidence-observer/pull/52)
  merged at `16d31f6ae1874389a6d5dc56e54997b88a7e2397`. Its installed offline
  reader checks the original Atomic run, source, executable, population and
  logs against consumer-selected pins. All 1,242 local tests passed. The
  dedicated PR and main workflows succeeded.
- [Atlas #18](https://github.com/probityai/agent-evidence-atlas/pull/18)
  merged at `1bdfce81e3afff4ec1c18afdfe3c7d423b693198`. The sixteenth Lab record
  retains the original four-case run and all nineteen original files, including
  its exact executable. All 174 local tests passed; main site and deployment
  workflows succeeded.
- [Vocabulary #12](https://github.com/probityai/agent-evidence-vocabulary/pull/12)
  is review-ready with twelve checks green. Its source-only crosswalk needs the
  independent evidence review required by its governance before merge.

## Published Atomic PRs

Three implemented proposals are published on `astrogilda/atomic`, based on
Atomic `dev` at `35b4b9ad56dd6e6dd1e824f89dd3afe1cbf5cdbc`:

| PR / branch | User-facing change | Verification |
| --- | --- | --- |
| [#234](https://github.com/atomicdotdev/atomic/pull/234), `feat/criterion-evidence-replay` | Optional native intent evidence replay with an explicitly selected local checker and an installed Verify recipe | 2,038 CLI tests, native signed-intent integration, stale/mutation controls, installed checker controls and strict Clippy passed |
| [#235](https://github.com/atomicdotdev/atomic/pull/235), `feat/provenance-dsse-export` | Optional exporter-signed provenance container and offline pinned-key reader | 169 canonical tests, two doctests, six actual CLI/Python tests, five ordinary compatibility tests and strict feature Clippy passed. Atomic-host interoperability CI passed |
| [#236](https://github.com/atomicdotdev/atomic/pull/236), `test/native-authority-recovery` | Four native local delegation cases across process restarts, retained executable/results and an installed-reader workflow | Original run: four cases and twelve worker observations. Full workspace on the retained source: 9,094 tests passed. Atomic-host installed consumer CI also passed; subsequent source checks are separately pinned in the state file |

The managed environment's direct submission attempts were historically refused
with `Resource not accessible by integration`, including when a user credential
was supplied. The user then configured an encrypted owned-fork Actions secret.
[The restricted hosted submission run](https://github.com/astrogilda/atomic/actions/runs/37077337708)
succeeded and opened all three review-ready PRs at their exact published heads.
Maintainer modifications are enabled. CI and reviews are tracked separately;
publication is not maintainer acceptance or completed adoption.

The one-shot active submission workflow is removed after successful use; its
reviewed inactive template remains in this checkpoint directory. The temporary
token and encrypted repository secret can now be revoked/removed by the user.
DSSE #235 and native #236 each passed all nine hosted checks, including the
Linux, macOS and Windows workspace tests. Criterion #234 at its current head
passed all eight checks, including Windows, on its one fresh CI attempt. The prior Windows
owner-startup failure and the single fresh attempt are recorded separately in
the state file. The author has no upstream push, maintain or admin
permission, so final upstream review and protected merge belong to Atomic.

## Scope and next boundary

The native evaluation establishes local certificate selection and persistence
across ordinary process restarts. It does not measure crash-mid-write recovery,
server authorization, remote revocation, external effects or model performance.
The installed reader does not execute a retained binary. All runs here remain
Probity-operated; outside repeated use and independent effect custody are not
established.

The eight component routes remain open: JCS admission is already a merged
runtime dependency; Vectors has its existing owned corpus PR; Verify, DSSE and
Observer now have executable contribution paths; Vocabulary supplies bounded
semantics; Atlas retains results; deployment Admission remains a conditional
consumer route requiring a real deployment host and appropriate evidence.

Preserve the full program's recorded 40% capability/integration, 25%
consumers/distribution, 20% review closure and 15% research/records allocation.
The October 8, October 31 and December 30 targets retain their recorded exact
definitions and remain goals. This lane does not change or claim completion of
them, narrow the seven tracks/eight components/five tiers, or take over active
release and upstream implementation owners.
