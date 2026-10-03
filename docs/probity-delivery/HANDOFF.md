# Resume the Atomic lane

Fetch Atomic's current `dev` and all three fork branches before proceeding.
Compare their heads against `DELIVERY-STATE.json`; do not assume this checkpoint
still names the latest source. The branch carrying this entrypoint is
`docs/probity-atomic-delivery-2026-10-02` on `astrogilda/atomic`.

The full private Probity execution branch remains owned by the other active
program session. This isolated checkpoint records public Atomic work without
editing that owner's entrypoint, action register or implementation branches.

## Shepherd the published proposals

Actual upstream PRs are #234 (criterion replay), #235 (DSSE export) and #236
(native evaluation and installed consumer), all review-ready with maintainer
modifications enabled. The hosted submission run37077337708 succeeded after the
user saved the encrypted Actions secret. Refresh those PRs and their current
heads first; do not create duplicates. Resolve actual CI failures and review
requests, preserving existing owners. Atomic's maintainers hold merge authority.

The following submission instructions and inactive workflow template preserve
the route for a fresh session; they are not instructions to submit these again.

### Windows result and bounded followup

The first #234 run at `403130f8df9964081094f32f58a70d53f13e419c` passed seven
checks but failed the existing Windows database-owner concurrency test:
`concurrent_stops_publish_ordered_ledgers_without_external_serialization`.
Prompt dispatch timed out waiting for the owner to become healthy. The test and
owner implementation were byte-unchanged from base; all Windows CLI unit tests
passed. A direct failed-job rerun was refused with HTTP403.

The documentation-only followup
`814c9aa5d77f07836a935acf216f7f2cfb2bdfd6` clarifies POSIX wrapper support,
Linux installed-checker results, macOS native integration and the lack of a
packaged Windows checker wrapper. Its ordinary PR synchronize event starts one
fresh full CI attempt. Run37078868661 then completed successfully: all eight
checks passed, including Windows. It does not claim to fix the owner failure. Keep the old
failure and new results separate. If the owner failure repeats, preserve the
active investigation rather than weaken checks or repeatedly reroll CI.

`vinceblock99` owns temporary Windows survey #237 at
`dcbc068517f83b514757633a289f3d25592fa6a7`, based on #207 and marked do-not-merge.
That survey does not change our PRs and must not be merged or duplicated by this
lane. The exact failure log digest and subsequent CI state are retained in
`DELIVERY-STATE.json`.

The `proposals/` directory retains the submitted descriptions. Do not reopen
these PRs or reactivate submission merely because a fresh environment has limited
access. The normal next step is CI and maintainer review on the existing PRs.

### Completed hosted submission route

Direct managed requests historically used App authentication even when a user
credential was supplied. The user configured the encrypted owned-fork Actions
secret `ATOMIC_UPSTREAM_PAT`; the restricted hosted submission run37077337708
succeeded, verified the exact heads and base, and created #234/#235/#236 with
maintainer modifications enabled. The credential value is not recorded here.

The active workflow was removed after its successful one-shot execution. Its
reviewed inactive template remains at
`docs/probity-delivery/submit-probity-atomic-prs.yml`. No credential is needed for
normal source pushes through the existing owned-fork access. User revocation of
the temporary token and deletion of its repository Actions secret are recommended
cleanup steps; completion has not been confirmed. The inactive helper and
`comparison-links.json` retain historical fallback preparation, not outstanding
submission work.

DSSE #235 and native #236 each passed all nine actual hosted checks, including
all three operating systems and their dedicated consumer jobs. They are open,
mergeable and have no reviews or comments at this checkpoint. The author has
read access but no upstream push, maintain or admin authority. Atomic maintainers
hold the next review and protected merge decision; green CI is separate from
acceptance, merging and recurring adoption.

Existing owner boundaries remain: #228 owns the canonical corpus job; #230/#231
own publication/database work; #207/#216 own the bridge and its failure harness;
#224 owns remote owner/external signing; #220/#221 own hook health/outcomes; #233
owns split/delete confirmation. These proposals do not replace those efforts.

The small shared Clippy correction is deliberately isolated in each lane. It
makes a private recursion-only helper associated and collapses an existing
conditional; it does not change native helper behavior. Preserve append-only
history and do not relabel earlier test results with later commit hashes.

## Preserve the original measurement

The original Atomic source is
`80be8dae6feb8b9106181b78fba2c7c6a7c8f4d8`, not necessarily the current proposal
head. Its report SHA-256 is
`92b6cd64f85e7a87e8aee04bff128195e578acfd969170c231bbb1ac55f5f7c1`.
Its binary SHA-256 is
`68bb01c3dffaf5fc6b9131ce22f330664b04c8f79f66d84150df6b8f8af33c`.

Atlas retains an author-assembled complete capsule at
[the immutable merged record](https://github.com/probityai/agent-evidence-atlas/tree/1bdfce81e3afff4ec1c18afdfe3c7d423b693198/experiments/atomic-delegation-2026-10-02).
The capsule is 2,126,900 bytes with SHA-256
`ba31071e1f0a60644117e3f4c300b16b07bc75c58caf3a5259047054132c3719`.
It preserves nineteen original files; it is not an original provider CI ZIP.

Observer's PR workflow and main workflow repeated a native execution and
installed-reader check. Their success is owned-host CI evidence, not recurring
Atomic-host adoption or an independently operated run. The GitHub PR artifact
metadata is retained separately. Download of its original provider archive was
refused by the storage endpoint, so no claim of local retention of that archive
is made.

## Review and adoption work that remains

Vocabulary #12 has a second-agent technical source audit but lacks the
independent review its governance requires. Do not call another Probity agent
an independent operator or fabricate a maintainer approval.

Seek a real Atomic reviewer/consumer for each implemented route through the PR
review process. Keep installation, host execution, maintainer acceptance,
recurring outside use and independent custody as separate facts. DSSE is
optional and does not replace Atomic's native signing scheme. Verify's four
bounded adapters do not establish arbitrary task correctness. The native reader
does not establish external effects merely because a local test passed.

Deployment Admission is still conditional: it needs a real downstream
deployment decision, authenticated evidence and a named policy host. No empty
dependency PR was created merely to count every component as adopted.

When new reviews, source changes or runs arrive, update the action register,
state and entrypoint together with exact commits, results and ownership.
