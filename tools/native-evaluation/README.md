# Native delegation evaluation

Run four native process-level evaluations and retain the original output,
test population, test executable, source commit and SHA-256 artifact manifest:

```sh
python3 tools/native-evaluation/run.py --output /tmp/atomic-delegation-run
```

The output directory must be new. The runner uses Python's standard library and
the repository's locked Rust dependencies. Set `CARGO_TARGET_DIR` as usual to
reuse a build cache. Each case runs the actual `atomic-canonical` and
`atomic-identity` public APIs; grant lookup happens in a fresh OS process with a
persisted identity store. It never touches the operator's installed identities.

The declared cases cover:

- Repeating an exact certificate write and restarting the reader preserves one
  certificate and its narrow permission, project, server and view scope.
- Local revocation remains effective after restart, while a distinct renewed
  certificate remains selectable.
- Expired, altered and ambiguous certificates cannot become active; adding a
  valid certificate restores a positive control without hiding corrupt files.
- Local self-contained selection accepts an internally valid certificate, but
  verification against an unrelated externally supplied issuer key fails. A
  grant for another subject cannot replace the selected agent's grant.

These are **local certificate selection and persistence** results. They do not
establish server permission enforcement, remote revocation, crash consistency
during a write, model behavior or external effect custody. Separate processes
still share one operator's clock, keys and filesystem. `report.json` retains
these limitations alongside results; an exit code alone is insufficient evidence.

The tests also run in the existing `cargo test --workspace` CI matrix. Ubuntu CI
retains a complete evaluation bundle using the same runner. A build failure or
timeout produces a failed report and preserved logs; it cannot count as a passed
native run. The helper test is ignored by the normal test runner and invoked only
by process-level cases.
