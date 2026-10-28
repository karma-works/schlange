#!/usr/bin/env python3
"""Sandbox driver: replays scenarios in the sandbox and records what the queue does.

    python3 -I sandbox/driver/sandbox.py doctor
    python3 -I sandbox/driver/sandbox.py run sandbox/scenarios/s01-single-pr.json --repo queue-lab
    python3 -I sandbox/driver/sandbox.py record <run-directory>     # record again
    python3 -I sandbox/driver/sandbox.py reset --repo queue-lab        # close open scenario PRs
    python3 -I sandbox/driver/sandbox.py token driver                  # print an installation token

Actors: "driver" is the driver app (bot), "human" is the account logged in with gh.
Recordings go to .sandbox-runs/ (not checked in): scenario, timeline per PR, queue PRs,
checks and the commits on main. This is the observed behaviour that Schlange is compared
against later.

Scenario format (JSON), steps in order:
  {"open": "a", "flags": ["fail-full"], "files": {...}, "draft": false, "datenschutz": true,
   "ki_review": "gruen"|"rot", "as": "driver"|"human", "conflict": true, "title": "..."}
  {"push": "a", "flags": [...], "files": {...}}        new commit on the PR branch
  {"comment": "a", "text": "@mergifyio queue", "as": "human"}
  {"label": "a", "add": "halt"} / {"label": "a", "remove": "halt"}
  {"ready": "a"}  /  {"draft": "a"}                    change the draft status
  {"edit_body": "a", "ki_review": "rot"}               set the description again
  {"close": "a"}
  {"main_commit": {"path": "content"}}                base moves outside the queue
  {"sleep": 30}
  {"wait": "merged"|"closed"|"done"|"queued", "prs": ["a"], "timeout": 2400}
  {"wait": "quiet", "seconds": 300, "timeout": 2400}   no activity on the PRs
  {"assert_unqueued": "a"}                            open and never entered the queue
Flags: see fixture/ci/run.sh. "conflict": true changes shared/conflict.txt (text conflict).
"""

import argparse
import base64
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("SCHLANGE_SANDBOX_CONFIG", Path.home() / ".config/schlange-sandbox"))
REPO_ROOT = Path(__file__).resolve().parents[2]
RUNS = Path(os.environ.get("SANDBOX_RUNS", REPO_ROOT / ".sandbox-runs"))
API = "https://api.github.com"
QUEUE_PREFIX = "mergify/merge-queue/"
# The driver acts with the sandbox credentials and must never touch another organisation, so the
# organisation is fixed; SANDBOX_ORG and --org can only repeat it.
ALLOWED_ORG = "schlange-sandbox"
DEFAULT_ORG = os.environ.get("SANDBOX_ORG", ALLOWED_ORG)


def die(msg):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(1)


def log(msg):
    print(f"[{datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- Access -----------------------------------------------------------------------------

def app_meta(role):
    f = CONFIG_DIR / "apps" / f"{role}.json"
    if not f.exists():
        die(f'App "{role}" is not registered (sandbox/setup.sh apps)')
    return json.loads(f.read_text())


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def app_jwt(role):
    meta = app_meta(role)
    t = int(time.time())
    head = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    body = b64url(json.dumps({"iat": t - 60, "exp": t + 540, "iss": meta["client_id"]}).encode())
    signing_input = f"{head}.{body}".encode()
    pem = CONFIG_DIR / "apps" / f"{role}.pem"
    sig = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", str(pem)], input=signing_input, capture_output=True, check=True
    ).stdout
    return f"{head}.{body}.{b64url(sig)}"


class Client:
    def __init__(self, token_fn, label):
        self.token_fn = token_fn
        self.label = label
        self._token = None
        self._expires = 0

    def token(self):
        if not self._token or time.time() > self._expires:
            self._token, ttl = self.token_fn()
            self._expires = time.time() + ttl
        return self._token

    def call(self, method, path, body=None, ok404=False, accept="application/vnd.github+json"):
        url = path if path.startswith("http") else API + path
        data = json.dumps(body).encode() if body is not None else None
        for attempt in range(5):
            req = urllib.request.Request(url, data=data, method=method, headers={
                "Accept": accept,
                "Authorization": f"Bearer {self.token()}",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "schlange-sandbox-driver",
            })
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    raw = resp.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as e:
                text = e.read().decode(errors="replace")
                if e.code == 404 and ok404:
                    return None
                if e.code in (502, 503, 504) or (e.code in (403, 429) and "rate limit" in text.lower()):
                    time.sleep(2 ** attempt * 5)
                    continue
                raise RuntimeError(f"{self.label}: {method} {path} → HTTP {e.code}") from None
        raise RuntimeError(f"{self.label}: {method} {path} failed repeatedly")

    def paged(self, path):
        sep = "&" if "?" in path else "?"
        page, out = 1, []
        while True:
            items = self.call("GET", f"{path}{sep}per_page=100&page={page}")
            if isinstance(items, dict):  # e.g. check-runs: {"check_runs": [...]}
                items = next(v for v in items.values() if isinstance(v, list))
            out += items
            if len(items) < 100:
                return out
            page += 1


def installation_client(role, owner=None):
    meta = app_meta(role)
    owner = owner or meta["owner"]
    kind = "orgs" if meta["owner_type"] == "org" else "users"

    def fetch():
        jwt = Client(lambda: (app_jwt(role), 500), f"{role}-jwt")
        inst = jwt.call("GET", f"/{kind}/{owner}/installation", ok404=True)
        if not inst:
            die(f'App "{role}" is not installed at {owner}: {meta["html_url"]}/installations/new')
        tok = jwt.call("POST", f"/app/installations/{inst['id']}/access_tokens")
        return tok["token"], 3000

    return Client(fetch, role)


def secret(env, filename):
    """Secret from the environment (CI) or from ~/.config/schlange-sandbox/<file> (local)."""
    if os.environ.get(env):
        return os.environ[env].strip()
    f = CONFIG_DIR / filename
    return f.read_text().strip() if f.exists() else None


def human_client():
    # Fine-grained token of karma-works, valid only for repos of schlange-sandbox.
    # Falling back to the gh login happens only locally and with a warning: it may do more than the sandbox.
    def fetch():
        tok = secret("SANDBOX_HUMAN_TOKEN", "human-token")
        if not tok:
            print("Warning: no sandbox token (human-token), using the gh login", file=sys.stderr)
            tok = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True, check=True).stdout.strip()
        return tok, 10 ** 9

    return Client(fetch, "human")


def mergify_client():
    key = secret("SANDBOX_MERGIFY_API_KEY", "mergify-api-key")
    if not key:
        return None
    c = Client(lambda: (key, 10 ** 9), "mergify")
    return c


MERGIFY_API = "https://api.mergify.com/v1"


def schlange_client():
    """Schlange's admin API (schlange-e2e in the runner): the same event log format as
    Mergify's, so both sides yield queued, enter_count and dequeue_codes."""
    url, token = os.environ.get("SCHLANGE_URL"), os.environ.get("SCHLANGE_ADMIN_TOKEN")
    if not url or not token or not url.startswith("http://127.0.0.1:"):
        return None, None
    return Client(lambda: (token, 10 ** 9), "schlange"), url


def fetch_events(client, url, strict=False):
    events, cursor = [], None
    for _ in range(50):
        page = client.call("GET", url + (f"&cursor={cursor}" if cursor else ""))
        if strict and (not isinstance(page, dict) or not isinstance(page.get("events"), list)):
            raise RuntimeError("invalid queue event log response")
        if strict and any(not isinstance(e, dict) or not isinstance(e.get("type"), str)
                          or ("queue" in e["type"] and e["type"].endswith("enter")
                              and not isinstance(e.get("pull_request"), int))
                          for e in page["events"]):
            raise RuntimeError("invalid queue event log entry")
        events += page.get("events", [])
        cursor = page.get("next_cursor") or (page.get("links") or {}).get("next")
        if strict and cursor and not page.get("events"):
            raise RuntimeError("incomplete queue event log response")
        if not cursor or not page.get("events"):
            break
    if strict and cursor:
        raise RuntimeError("queue event log exceeds pagination limit")
    return events


def currently_queued(events, number):
    mine = [e["type"] for e in events if e.get("pull_request") == number and "queue" in e["type"]]
    return sum(t.endswith("enter") for t in mine) > sum(t.endswith("leave") for t in mine)


# --- Scenario ---------------------------------------------------------------------------

def pr_body(spec, run_id, key):
    parts = [f"Sandbox scenario `{run_id}`, PR `{key}`."]
    if spec.get("datenschutz", True):
        parts.append("## Datenschutz\n\nNo personal data (test data).")
    if spec.get("ki_review") == "rot":
        parts.append("KI-Review: rot")
    return "\n\n".join(parts)


class Run:
    def __init__(self, org, repo, scenario, out_dir):
        self.org, self.repo = org, repo
        self.r = f"/repos/{org}/{repo}"
        self.scenario = scenario
        self.run_id = out_dir.name
        self.out = out_dir
        self.actors = {"driver": installation_client("driver", org), "human": human_client()}
        self.prs = {}  # key -> {"number", "branch"}
        self.checkpoints = []
        self.timed_out = False
        self.started = now_iso()

    def actor(self, spec):
        return self.actors[spec.get("as", "driver")]

    def flag_path(self, key):
        return f"ci/flags/{self.run_id}-{key}"

    def flag_lines(self, flags):
        out = []
        for f in flags:
            # conflicts-with <key> refers to the flag file of the other scenario PR.
            if f.startswith("conflicts-with "):
                out.append(f"conflicts-with {self.run_id}-{f.split(' ', 1)[1]}")
            else:
                out.append(f)
        return "\n".join(out) + "\n"

    def commit_files(self, client, branch, files, msg):
        """One commit with all files on the branch (one push, one synchronize event)."""
        ref = client.call("GET", f"{self.r}/git/ref/heads/{branch}")
        parent = ref["object"]["sha"]
        base_tree = client.call("GET", f"{self.r}/git/commits/{parent}")["tree"]["sha"]
        tree = client.call("POST", f"{self.r}/git/trees", {"base_tree": base_tree, "tree": [
            {"path": path, "mode": "100644", "type": "blob", "content": content} for path, content in files.items()
        ]})
        commit = client.call("POST", f"{self.r}/git/commits", {"message": msg, "tree": tree["sha"], "parents": [parent]})
        client.call("PATCH", f"{self.r}/git/refs/heads/{branch}", {"sha": commit["sha"], "force": False})
        return commit["sha"]

    def write_changes(self, client, branch, key, spec, n):
        files = {path: content.replace("{run}", self.run_id) for path, content in spec.get("files", {}).items()}
        # Every commit changes the flag file, so CI always has something to evaluate.
        files[self.flag_path(key)] = f"# {self.run_id} {key} commit {n}\n" + self.flag_lines(spec.get("flags", []))
        if spec.get("conflict"):
            files["shared/conflict.txt"] = f"Shared file for text conflicts between scenario PRs.\nline: {key} ({self.run_id})\n"
        self.commit_files(client, branch, files, f"Scenario {self.run_id}: {key} ({n})")

    def step_open(self, s):
        key, client = s["open"], self.actor(s)
        branch = f"sbx/{self.run_id}/{key}"
        main = client.call("GET", f"{self.r}/git/ref/heads/main")
        client.call("POST", f"{self.r}/git/refs", {"ref": f"refs/heads/{branch}", "sha": main["object"]["sha"]})
        self.write_changes(client, branch, key, s, 1)
        pr = client.call("POST", f"{self.r}/pulls", {
            "title": s.get("title", f"Scenario {self.scenario['id']}: {key}"),
            "head": branch, "base": "main", "body": pr_body(s, self.run_id, key), "draft": s.get("draft", False),
        })
        self.prs[key] = {"number": pr["number"], "branch": branch, "commits": 1, "node_id": pr["node_id"]}
        log(f"PR {key} = #{pr['number']} ({s.get('as', 'driver')})")

    def step_push(self, s):
        key = s["push"]
        p = self.prs[key]
        p["commits"] += 1
        self.write_changes(self.actor(s), p["branch"], key, s, p["commits"])
        log(f"Push to {key} (#{p['number']})")

    def step_comment(self, s):
        n = self.prs[s["comment"]]["number"]
        self.actor(s).call("POST", f"{self.r}/issues/{n}/comments", {"body": s["text"]})
        log(f"Comment on #{n} as {s.get('as', 'driver')}: {s['text']}")

    def step_label(self, s):
        n = self.prs[s["label"]]["number"]
        c = self.actor(s)
        if "add" in s:
            c.call("POST", f"{self.r}/issues/{n}/labels", {"labels": [s["add"]]})
        else:
            c.call("DELETE", f"{self.r}/issues/{n}/labels/{s['remove']}", ok404=True)
        log(f"Label on #{n}: {s}")

    def graphql(self, client, query, variables):
        res = client.call("POST", "/graphql", {"query": query, "variables": variables})
        if res.get("errors"):
            raise RuntimeError(res["errors"])

    def step_ready(self, s):
        p = self.prs[s["ready"]]
        self.graphql(self.actor(s), "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){clientMutationId}}", {"id": p["node_id"]})
        log(f"#{p['number']} ready")

    def step_draft(self, s):
        p = self.prs[s["draft"]]
        self.graphql(self.actor(s), "mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){clientMutationId}}", {"id": p["node_id"]})
        log(f"#{p['number']} draft again")

    def step_edit_body(self, s):
        key = s["edit_body"]
        n = self.prs[key]["number"]
        self.actor(s).call("PATCH", f"{self.r}/pulls/{n}", {"body": pr_body(s, self.run_id, key)})
        log(f"Description of #{n} changed")

    def step_close(self, s):
        n = self.prs[s["close"]]["number"]
        self.actor(s).call("PATCH", f"{self.r}/pulls/{n}", {"state": "closed"})
        log(f"#{n} closed")

    def step_main_commit(self, s):
        self.checkpoints.append({"kind": "base_push_started", "at": now_iso()})
        self.save_state()
        files = {path: content.replace("{run}", self.run_id) for path, content in s["main_commit"].items()}
        self.commit_files(self.actors["human"], "main", files, f"Scenario {self.run_id}: base moves")
        self.checkpoints.append({"kind": "base_push_completed", "at": now_iso()})
        self.save_state()
        log("Commit directly on main (admin bypass)")

    def queue_activity(self):
        h = self.actors["human"]
        pulls = h.call("GET", f"{self.r}/pulls?state=open&per_page=100")
        queue = sorted((p["number"], p["head"]["sha"]) for p in pulls if p["head"]["ref"].startswith("mergify/merge-queue/"))
        main = h.call("GET", f"{self.r}/git/ref/heads/main")["object"]["sha"]
        return json.dumps([queue, main])

    def pr_state(self, key):
        pr = self.actors["human"].call("GET", f"{self.r}/pulls/{self.prs[key]['number']}")
        return "merged" if pr["merged"] else pr["state"], pr["updated_at"]

    def queue_events(self):
        if self.repo == "queue-lab":
            client, base = mergify_client(), MERGIFY_API
        else:
            client, base = schlange_client()
        if client is None or base is None:
            raise RuntimeError("queue event log is required for queue checkpoints")
        url = f"{base}/repos/{self.org}/{self.repo}/logs?per_page=100&received_from={self.started}"
        return fetch_events(client, url, strict=True)

    def step_assert_unqueued(self, s):
        key = s["assert_unqueued"]
        number = self.prs[key]["number"]
        entered = any(e.get("pull_request") == number and "queue" in e.get("type", "")
                      and e["type"].endswith("enter") for e in self.queue_events())
        if self.pr_state(key)[0] != "open" or entered:
            raise RuntimeError(f"PR {key} was closed or entered the queue before the authorized command")
        log(f"PR {key} is open and has not entered the queue")

    def step_wait(self, s):
        what, deadline = s["wait"], time.time() + s.get("timeout", 2400)
        keys = s.get("prs", list(self.prs))
        last_change, seen = time.time(), {}
        while time.time() < deadline:
            states = {k: self.pr_state(k) for k in keys}
            if what == "quiet":
                # Quiet means the queue is idle: also open queue pull requests and the tip of
                # main count as activity. A queue that updates the scenario pull requests rarely
                # (checks run for minutes) is not idle while it still tests batches.
                states["_queue"] = (self.queue_activity(), "")
            if states != seen:
                seen, last_change = states, time.time()
                log("State: " + ", ".join(f"{k}=#{self.prs[k]['number']}:{v[0]}" for k, v in states.items() if k in self.prs))
            st = {k: v[0] for k, v in states.items()}
            if what == "queued":
                events = self.queue_events()
                if all(st[k] == "open" and currently_queued(events, self.prs[k]["number"]) for k in keys):
                    numbers = {self.prs[k]["number"] for k in keys}
                    self.checkpoints.append({"kind": "queued", "at": now_iso(), "prs": keys,
                        "events": [{"pull_request": e["pull_request"], "type": e["type"],
                                    "received_at": e.get("received_at")}
                                   for e in events if e.get("pull_request") in numbers
                                   and "queue" in e["type"] and (e["type"].endswith("enter")
                                                                 or e["type"].endswith("leave"))]})
                    log("Queue checkpoint: " + ", ".join(keys))
                    return
                if any(st[k] != "open" for k in keys):
                    raise RuntimeError("pull request closed before the queue checkpoint")
            if what == "merged" and all(v == "merged" for v in st.values()):
                return
            if what == "closed" and all(v == "closed" for v in st.values()):
                return
            if what == "done" and all(v in ("merged", "closed") for v in st.values()):
                return
            if what == "quiet" and time.time() - last_change >= s.get("seconds", 300):
                return
            time.sleep(20)
        if what == "queued":
            raise RuntimeError("timeout before queue checkpoint; subsequent steps were not run")
        log(f"Timeout while waiting for {what}; recording anyway")
        self.timed_out = True

    def execute(self):
        (self.out / "scenario.json").write_text(json.dumps(self.scenario, indent=2, ensure_ascii=False))
        for s in self.scenario["steps"]:
            kind = next(k for k in ("open", "push", "comment", "label", "ready", "draft", "edit_body",
                                    "close", "main_commit", "sleep", "wait", "assert_unqueued") if k in s)
            if kind == "sleep":
                time.sleep(s["sleep"])
            else:
                getattr(self, f"step_{kind}")(s)
            self.save_state()
        self.save_state(finished=now_iso())

    def save_state(self, **extra):
        state = {"org": self.org, "repo": self.repo, "run_id": self.run_id, "started": self.started,
                 "prs": self.prs, "checkpoints": self.checkpoints, "timed_out": self.timed_out, **extra}
        (self.out / "run.json").write_text(json.dumps(state, indent=2))


# --- Recording -----------------------------------------------------------------------

def public_record_summary(summary):
    """Print only structured facts; upstream text stays in the private local summary."""
    conclusions = {"success", "failure", "neutral", "cancelled", "skipped", "timed_out", "action_required", "stale"}
    return {"prs": {k: {"number": pr["number"], "end": pr["end"]}
                    for k, pr in summary["prs"].items()},
            "queue_prs": [{"number": q["number"], "commits": q["commits"],
                           "ci_ok": q["ci_ok"] if q["ci_ok"] in conclusions else None}
                          for q in summary["queue_prs"]],
            "main_commit_count": len(summary["main_commits"])}


def record(out):
    state = json.loads((out / "run.json").read_text())
    c = human_client()
    r = f"/repos/{state['org']}/{state['repo']}"
    trace = {"run": state, "recorded": now_iso(), "prs": {}, "queue_prs": [], "main": []}
    for key, p in state["prs"].items():
        n = p["number"]
        pr = c.call("GET", f"{r}/pulls/{n}")
        timeline = c.paged(f"{r}/issues/{n}/timeline")
        trace["prs"][key] = {
            "number": n, "state": pr["state"], "merged": pr["merged"], "merged_at": pr["merged_at"],
            "merged_by": (pr.get("merged_by") or {}).get("login"), "merge_commit_sha": pr["merge_commit_sha"],
            "head_sha": pr["head"]["sha"], "labels": [lb["name"] for lb in pr["labels"]],
            "timeline": [slim_event(e) for e in timeline],
            "checks": [slim_check(x) for x in c.paged(f"{r}/commits/{pr['head']['sha']}/check-runs")],
        }
    for pr in c.paged(f"{r}/pulls?state=all&sort=created&direction=desc"):
        if pr["created_at"] < state["started"]:
            break
        if pr["head"]["ref"].startswith(QUEUE_PREFIX):
            trace["queue_prs"].append({
                "number": pr["number"], "head": pr["head"]["ref"], "user": pr["user"]["login"],
                "title": pr["title"],
                "state": pr["state"], "draft": pr["draft"], "created_at": pr["created_at"],
                "closed_at": pr["closed_at"], "body": pr["body"],
                "commits": [x["commit"]["message"].split("\n")[0] for x in c.paged(f"{r}/pulls/{pr['number']}/commits")],
                "checks": [slim_check(x) for x in c.paged(f"{r}/commits/{pr['head']['sha']}/check-runs")],
            })
    for cm in c.paged(f"{r}/commits?sha=main&since={state['started']}"):
        trace["main"].append({"sha": cm["sha"], "tree": cm["commit"]["tree"]["sha"],
                              "author": (cm.get("author") or {}).get("login"),
                              "committer": (cm.get("committer") or {}).get("login"),
                              "message": cm["commit"]["message"], "date": cm["commit"]["committer"]["date"]})
    # Mergify's activity log (queue-lab only): structured reasons for entering and leaving the queue.
    m = mergify_client() if state["repo"] == "queue-lab" else None
    if m:
        url = (f"{MERGIFY_API}/repos/{state['org']}/{state['repo']}/logs?per_page=100"
               f"&received_from={state['started']}")
        trace["mergify_events"] = fetch_events(m, url)
    # Schlange's event log (schlange-e2e): the same format, from its admin API in the runner.
    sc, base = schlange_client() if state["repo"] == "schlange-e2e" else (None, None)
    if sc:
        url = (f"{base}/repos/{state['org']}/{state['repo']}/logs?per_page=100"
               f"&received_from={state['started']}")
        trace["queue_events"] = fetch_events(sc, url)
    (out / "trace.json").write_text(json.dumps(trace, indent=2, ensure_ascii=False))
    summary = {
        "prs": {k: {"number": v["number"], "end": "merged" if v["merged"] else v["state"], "labels": v["labels"],
                    "bot_comments": [e["body"][:200] for e in v["timeline"]
                                     if e.get("event") == "commented" and (e.get("actor") or "").endswith("[bot]")]}
                for k, v in trace["prs"].items()},
        "queue_prs": [{"number": q["number"], "commits": len(q["commits"]),
                       "ci_ok": next((x["conclusion"] for x in q["checks"] if x["name"] == "ci-ok"), None)}
                      for q in reversed(trace["queue_prs"])],
        "main_commits": [m["message"].split("\n")[0] for m in reversed(trace["main"])],
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False))
    print(json.dumps(public_record_summary(summary), indent=2, ensure_ascii=False))
    log(f"Recording: {out}")


# --- Outcome and comparison -------------------------------------------------------------
# outcome.json is a format of its own, free of text (no Mergify texts, clean room), and can be
# checked in as a baseline: sandbox/baselines/<scenario>.json.

def write_outcome(out):
    trace = json.loads((out / "trace.json").read_text())
    key_of = {v["number"]: k for k, v in trace["prs"].items()}
    events = trace.get("queue_events", trace.get("mergify_events"))
    prs = {}
    for k, v in trace["prs"].items():
        n = v["number"]
        mine = [e for e in events or [] if e.get("pull_request") == n]
        entered = [e for e in mine if "queue" in e["type"] and e["type"].endswith("enter")]
        left = [e for e in mine if "queue" in e["type"] and e["type"].endswith("leave")]
        prs[k] = {
            "end": "merged" if v["merged"] else v["state"],
            # From the queue's event log: Mergify's (queue-lab) or Schlange's (schlange-e2e).
            "queued": bool(entered) if events is not None else None,
            "enter_count": len(entered) if events is not None else None,
            "dequeue_codes": [(e.get("metadata") or {}).get("dequeue_code") for e in left
                              if not (e.get("metadata") or {}).get("merged")] if events is not None else None,
        }
    landed = []
    for cm in reversed(trace["main"]):  # oldest first
        for num in re.findall(r"\(#(\d+)\)", cm["message"].split("\n")[0]):
            if int(num) in key_of:
                landed.append(key_of[int(num)])
    batches = []
    for q in reversed(trace["queue_prs"]):
        # The pull requests embarked in this queue pull request: the title names them; the
        # body also lists the pull requests ahead, so it is only a fallback for old traces.
        text = q.get("title") if q.get("title") is not None else (q.get("body") or "")
        keys = sorted({key_of[int(x)] for x in re.findall(r"#(\d+)", text) if int(x) in key_of})
        ci = next((x["conclusion"] for x in q["checks"] if x["name"] == "ci-ok"), None)
        batches.append({"prs": keys, "ci_ok": ci})
    outcome = {"scenario": trace["run"]["run_id"].split("-")[0].upper(), "repo": trace["run"]["repo"],
               "prs": prs, "landed": landed, "batches": batches}
    (out / "outcome.json").write_text(json.dumps(outcome, indent=2, ensure_ascii=False) + "\n")
    return outcome


def compare_outcomes(expected_path, actual_path, strict_batches=False, strict_observation=False, expected_prs=None):
    """0 = same behaviour, 1 = deviations (they are printed)."""
    exp = json.loads(Path(expected_path).read_text())
    act = json.loads(Path(actual_path).read_text())
    if not isinstance(exp, dict) or not isinstance(act, dict):
        print("DEVIATION\n  outcome is not an object")
        return 1
    strict_observation = strict_observation or act.get("repo") == "schlange-e2e"
    if strict_observation:
        if (act.get("repo") != "schlange-e2e" or not isinstance(exp.get("prs"), dict)
                or not exp["prs"] or not isinstance(act.get("prs"), dict) or not act["prs"]
                or not all(isinstance(k, str) and isinstance(v, dict)
                           for prs in (exp["prs"], act["prs"]) for k, v in prs.items())
                or not all(isinstance(x, list) and all(isinstance(k, str) for k in x)
                           for x in (exp.get("landed"), act.get("landed")))):
            print("DEVIATION\n  compatibility evidence is missing or invalid")
            return 1
        if expected_prs is not None and (set(exp["prs"]) != expected_prs or set(act["prs"]) != expected_prs):
            print("DEVIATION\n  pull request evidence does not match the scenario")
            return 1
    diffs, known = [], []
    # Documented deviations of Schlange from the baseline: {"pr", "field", "observed", "why"}.
    allowed = {(d["pr"], d["field"]): d for d in exp.get("known_deviations", [])}
    def valid(field, value):
        return {
            "end": lambda v: v in ("open", "closed", "merged"),
            "queued": lambda v: type(v) is bool,
            "enter_count": lambda v: type(v) is int and v >= 0,
            "dequeue_codes": lambda v: isinstance(v, list) and all(isinstance(c, str) for c in v),
        }[field](value)
    for k in sorted(set(exp["prs"]) | set(act["prs"])):
        e, a = exp["prs"].get(k, {}), act["prs"].get(k, {})
        for field in ("end", "queued", "enter_count", "dequeue_codes"):
            if strict_observation and (not valid(field, e.get(field)) or not valid(field, a.get(field))):
                diffs.append(f"PR {k}.{field}: compatibility evidence is missing or invalid")
                continue
            # None means: this side cannot observe the field.
            if e.get(field) is not None and a.get(field) is not None and e.get(field) != a.get(field):
                line = f"PR {k}.{field}: expected {e.get(field)}, observed {a.get(field)}"
                d = allowed.get((k, field))
                if d and d.get("observed") == a.get(field):
                    known.append(f"{line} ({d['why']})")
                else:
                    diffs.append(line)
    if exp["landed"] != act["landed"]:
        line = f"Landing order: expected {exp['landed']}, observed {act['landed']}"
        d = allowed.get(("*", "landed"))
        if d and d.get("observed") == act["landed"]:
            known.append(f"{line} ({d['why']})")
        else:
            diffs.append(line)
    # Batches depend on timing (batch_max_wait_time); compare strictly only on request.
    if strict_batches and exp.get("batches") is not None and \
            [b["prs"] for b in exp["batches"]] != [b["prs"] for b in act["batches"]]:
        diffs.append(f"Batches: expected {exp['batches']}, observed {act['batches']}")
    if known:
        print("KNOWN DEVIATION (documented)\n  " + "\n  ".join(known))
    if diffs:
        print("DEVIATION\n  " + "\n  ".join(diffs))
        return 1
    print("SAME")
    return 0


def slim_event(e):
    actor = (e.get("actor") or e.get("user") or {}).get("login")
    out = {"event": e.get("event"), "at": e.get("created_at") or e.get("submitted_at"), "actor": actor}
    if e.get("label"):
        out["label"] = e["label"]["name"]
    if e.get("body"):
        out["body"] = e["body"]
    if e.get("sha"):
        out["sha"] = e["sha"]
    if e.get("source"):
        out["source"] = (e["source"].get("issue") or {}).get("number")
    return out


def slim_check(x):
    return {"name": x["name"], "status": x["status"], "conclusion": x["conclusion"],
            "app": (x.get("app") or {}).get("slug"), "started_at": x["started_at"], "completed_at": x["completed_at"]}


# --- Commands ----------------------------------------------------------------------------

def busy(org, repo):
    """Open scenario or queue PRs: then something is running there (local, CI or another agent)."""
    open_prs = human_client().paged(f"/repos/{org}/{repo}/pulls?state=open")
    return [p["number"] for p in open_prs if p["head"]["ref"].startswith(("sbx/", QUEUE_PREFIX))]


def wait_idle(org, repo, timeout):
    # Two scenarios at once in one repo end up in the same batch and distort each other.
    deadline = time.time() + timeout
    while (prs := busy(org, repo)):
        if time.time() > deadline:
            die(f"{org}/{repo} is busy (open PRs {prs}); clean up with: sandbox.py reset --repo {repo}")
        log(f"{repo} is busy (open PRs {prs}), waiting …")
        time.sleep(30)


def cmd_run(a):
    scenario = json.loads(Path(a.scenario).read_text())
    if not a.no_wait:
        wait_idle(a.org, a.repo, a.idle_timeout)
    run_id = f"{scenario['id'].lower()}-{a.repo}-{datetime.now().strftime('%Y%m%d-%H%M%S')}"
    out = RUNS / run_id
    out.mkdir(parents=True)
    log(f"Scenario {scenario['id']} – {scenario['title']} in {a.org}/{a.repo} ({out})")
    run = Run(a.org, a.repo, scenario, out)
    try:
        run.execute()
    finally:
        record(out)
        if not a.keep:
            cleanup(a.org, a.repo, f"sbx/{out.name}/")
    write_outcome(out)
    # For schlange-e2e a timeout means the queue did not do its job; without a baseline there
    # would be nothing else to fail the run (SANDBOX_FAIL_ON_TIMEOUT=1 from run-scenarios.sh).
    if getattr(run, "timed_out", False) and os.environ.get("SANDBOX_FAIL_ON_TIMEOUT") == "1":
        log("A wait timed out: failing the run")
        sys.exit(1)
    if a.baseline:
        expected_prs = {step["open"] for step in scenario["steps"] if "open" in step}
        sys.exit(compare_outcomes(Path(a.baseline), out / "outcome.json",
                                  strict_observation=a.repo == "schlange-e2e", expected_prs=expected_prs))


def cleanup(org, repo, prefix="sbx/"):
    """Closes open scenario PRs and deletes their branches (after every run, otherwise the repo stays locked)."""
    h = human_client()
    r = f"/repos/{org}/{repo}"
    for pr in h.paged(f"{r}/pulls?state=open"):
        if pr["head"]["ref"].startswith(prefix):
            h.call("PATCH", f"{r}/pulls/{pr['number']}", {"state": "closed"})
            log(f"#{pr['number']} closed")
    for ref in h.paged(f"{r}/git/matching-refs/heads/{prefix}"):
        h.call("DELETE", f"{r}/git/{ref['ref']}", ok404=True)
        log(f"{ref['ref']} deleted")


def cmd_reset(a):
    cleanup(a.org, a.repo)


def cmd_doctor(a):
    ok = True
    for role in ("queue", "driver"):
        f = CONFIG_DIR / "apps" / f"{role}.json"
        if not f.exists():
            print(f"☐ App {role} not registered")
            ok = False
            continue
        try:
            jwt = Client(lambda r=role: (app_jwt(r), 500), role)
            inst = jwt.call("GET", f"/orgs/{a.org}/installation", ok404=True)
            if not inst:
                print(f"☐ App {role} not installed in {a.org}: {app_meta(role)['html_url']}/installations/new")
                ok = False
                continue
            tok = jwt.call("POST", f"/app/installations/{inst['id']}/access_tokens")["token"]
            repos = Client(lambda t=tok: (t, 3000), role).call("GET", "/installation/repositories")
            names = sorted(x["name"] for x in repos["repositories"])
            print(f"→ App {role} installed, repos: {', '.join(names) or '–'}")
        except Exception as e:  # noqa: BLE001
            print(f"☐ App {role}: {e}")
            ok = False
    sys.exit(0 if ok else 1)


def cmd_token(a):
    print(installation_client(a.role, a.org).token())


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--org", default=DEFAULT_ORG)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("scenario")
    r.add_argument("--repo", required=True, choices=["queue-lab", "schlange-e2e"])
    r.add_argument("--baseline", help="outcome.json as the baseline; exit 1 on deviation")
    r.add_argument("--no-wait", action="store_true", help="do not wait until the repo is free")
    r.add_argument("--keep", action="store_true", help="do not close open scenario PRs after the run")
    r.add_argument("--idle-timeout", type=int, default=1800)
    sub.add_parser("record").add_argument("dir")
    sub.add_parser("outcome").add_argument("dir")
    c = sub.add_parser("compare")
    c.add_argument("expected")
    c.add_argument("actual")
    c.add_argument("--strict-batches", action="store_true")
    c.add_argument("--strict-observation", action="store_true", help="require complete Schlange compatibility evidence")
    sub.add_parser("reset").add_argument("--repo", required=True)
    sub.add_parser("doctor")
    sub.add_parser("token").add_argument("role", choices=["queue", "driver", "shadow"])
    # also allow --org after the subcommand
    for sp in sub.choices.values():
        sp.add_argument("--org", default=argparse.SUPPRESS)
    a = p.parse_args()
    if getattr(a, "org", ALLOWED_ORG) != ALLOWED_ORG:
        sys.exit(f"Refused: only the organisation {ALLOWED_ORG} may be used, not {a.org}")
    if getattr(a, "repo", None) not in (None, "queue-lab", "schlange-e2e"):
        sys.exit(f"Refused: unknown sandbox repository {a.repo}")
    {"run": cmd_run, "record": lambda a: record(Path(a.dir)), "reset": cmd_reset,
     "outcome": lambda a: print(json.dumps(write_outcome(Path(a.dir)), indent=2, ensure_ascii=False)),
     "compare": lambda a: sys.exit(compare_outcomes(a.expected, a.actual, a.strict_batches, a.strict_observation)),
     "doctor": cmd_doctor, "token": cmd_token}[a.cmd](a)


if __name__ == "__main__":
    main()
