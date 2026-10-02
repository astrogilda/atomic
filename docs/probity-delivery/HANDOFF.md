# Resume the Atomic lane

Fetch Atomic's current `dev` and all three fork branches before proceeding.
Compare their heads against `DELIVERY-STATE.json`; do not assume this checkpoint
still names the latest source. The branch carrying this entrypoint is
`docs/probity-atomic-delivery-2026-10-02` on `astrogilda/atomic`.

The full private Probity execution branch remains owned by the other active
program session. This isolated checkpoint records public Atomic work without
editing that owner's entrypoint, action register or implementation branches.

## Submit the implemented proposals

The `proposals/` directory retains review-ready descriptions. Once a usable
credential is available, first verify its effective GitHub identity and public
contribution access through the approved transport. Do not expose credentials in
logs, commit them or bypass the network configuration.

Create each PR in `atomicdotdev/atomic` with base `dev` and the corresponding
head `astrogilda:<branch>`. Refresh existing PRs first to prevent duplicates.
Set maintainer modification permission and preserve the bounded descriptions.
Follow actual CI and maintainer review; opening a PR is not completed adoption.
Normal upstream merge authority belongs to Atomic's maintainers.

### Supported hosted submission route

The current managed transport continued using App authentication when a user
credential was supplied, refusing the criterion and DSSE submissions. The
secret value is not recorded here. The owned fork's App access also lacks
repository secret management permission.

The user can add an encrypted repository Actions secret named
`ATOMIC_UPSTREAM_PAT` through
[the fork's secret settings](https://github.com/astrogilda/atomic/settings/secrets/actions/new)
and enable workflows through
[the fork's Actions tab](https://github.com/astrogilda/atomic/actions).
Add the secret before queuing the job. Once the user confirms configuration,
publish the prepared `submit-probity-atomic-prs.yml` workflow on this checkpoint
branch and make a normal fast-forward push to trigger it. No default-branch
merge or GitHub App installation in Atomic is required for that trigger.

The fixed submission helper checks effective user/scopes, all three published
head SHAs, existing PRs and returned upstream head/base before recording URLs.
It creates only the three listed review-ready PRs and does not request merges.
Its credential is available only to that API step. Observe the actual run and
upstream PRs; a prepared workflow is not a submitted PR. Partial progress is
safe to resume because each lane checks for an existing matching PR first.

`python3 docs/probity-delivery/submit_prs.py` prints an offline plan.
`--execute` requires the user secret and runs the requests. If hosted submission
is unavailable, `comparison-links.json` contains prefilled GitHub browser forms.
After submission, revoke the temporary credential and remove the temporary
Actions secret through the user's settings.

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
