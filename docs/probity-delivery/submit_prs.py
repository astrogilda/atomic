#!/usr/bin/env python3
"""Submit the three fixed, reviewed Atomic proposals using user authentication.

Runs on a GitHub-hosted runner with a repository Actions secret. No credential
is accepted from a document, argument, producer packet or untrusted event.
"""

import argparse
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent
BASE_REPO = "atomicdotdev/atomic"
FORK = "astrogilda/atomic"
ACTOR = "astrogilda"
LANES = {
    "criterion": ("feat/criterion-evidence-replay", "feat(intent): replay bounded retained evidence with native freshness pins"),
    "dsse": ("feat/provenance-dsse-export", "feat(provenance): add optional DSSE export and pinned-key verification"),
    "native": ("test/native-authority-recovery", "test: retain native delegation evaluation with an installed offline consumer"),
}


def proposals():
    state = json.loads((ROOT / "DELIVERY-STATE.json").read_text())
    if state["upstream_base"]["repository"] != BASE_REPO or state["upstream_base"]["branch"] != "dev":
        raise ValueError("unexpected upstream target")
    rows = state["proposals"]
    if len(rows) != len(LANES) or {r["lane"] for r in rows} != set(LANES):
        raise ValueError("unexpected proposal population")
    result = []
    for row in rows:
        branch, title = LANES[row["lane"]]
        if row["branch"] != branch or not row["fork_published"] or not re.fullmatch(r"[0-9a-f]{40}", row["head"]):
            raise ValueError("proposal branch or immutable publication pin mismatch")
        body = (ROOT / "proposals" / (row["lane"] + ".md")).read_text()
        if not body.strip() or len(body.encode()) > 65536:
            raise ValueError("missing or oversized review description")
        result.append((row, {"title": title, "head": ACTOR + ":" + branch,
                             "base": "dev", "body": body, "draft": False,
                             "maintainer_can_modify": True}))
    return result


def api(credential, path, data=None):
    request = urllib.request.Request(
        "https://api.github.com/" + path,
        data=None if data is None else json.dumps(data).encode(),
        headers={"Authorization": "Bearer " + credential,
                 "Accept": "application/vnd.github+json",
                 "Content-Type": "application/json",
                 "X-GitHub-Api-Version": "2022-11-28"},
    )
    try:
        response = urllib.request.urlopen(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            raise ValueError("API response exceeds budget")
        value = json.loads(raw)
        if not 200 <= response.status < 300:
            # Print the service's fixed message, never headers or credentials.
            raise ValueError("GitHub HTTP " + str(response.status) + ": " + str(value.get("message", "request refused")))
        return value, response.headers


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute", action="store_true")
    args = parser.parse_args()
    plans = proposals()
    if not args.execute:
        print(json.dumps({"mode": "offline-plan", "repository": BASE_REPO,
                          "proposals": [{"lane": row["lane"], "head": payload["head"],
                                         "sha": row["head"], "title": payload["title"]}
                                        for row, payload in plans]}, indent=2))
        return 0
    credential = os.environ.get("ATOMIC_UPSTREAM_PAT", "")
    if not credential:
        raise ValueError("configure the ATOMIC_UPSTREAM_PAT Actions secret first")
    actor, headers = api(credential, "user")
    scopes = {s.strip() for s in headers.get("X-OAuth-Scopes", "").split(",")}
    if actor.get("login") != ACTOR or not scopes.intersection({"public_repo", "repo"}):
        raise ValueError("effective credential is not the expected scoped user credential")
    results = []
    for row, payload in plans:
        branch = urllib.parse.quote(row["branch"], safe="/")
        ref, _ = api(credential, "repos/" + FORK + "/git/ref/heads/" + branch)
        if ref["object"]["sha"] != row["head"]:
            raise ValueError("fork branch changed; refresh checkpoint before submission")
        query = urllib.parse.urlencode({"state": "open", "head": payload["head"], "base": "dev"})
        existing, _ = api(credential, "repos/" + BASE_REPO + "/pulls?" + query)
        if len(existing) > 1:
            raise ValueError("multiple matching upstream PRs require review")
        if existing:
            pull = existing[0]
            disposition = "existing"
        else:
            pull, _ = api(credential, "repos/" + BASE_REPO + "/pulls", payload)
            disposition = "created"
        if pull["head"]["sha"] != row["head"] or pull["base"]["repo"]["full_name"] != BASE_REPO or pull["base"]["ref"] != "dev":
            raise ValueError("upstream readback differs from reviewed proposal")
        result = {"lane": row["lane"], "disposition": disposition,
                  "number": pull["number"], "url": pull["html_url"], "head": pull["head"]["sha"]}
        results.append(result)
        print(json.dumps(result), flush=True)
    Path("atomic-upstream-prs.json").write_text(json.dumps(results, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, KeyError, urllib.error.URLError) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1)
