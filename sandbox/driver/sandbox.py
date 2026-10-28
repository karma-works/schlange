#!/usr/bin/env python3
"""Sandbox-Driver: spielt Szenarien in der Sandbox ab und zeichnet auf, was die Queue tut.

    python3 -I sandbox/driver/sandbox.py doctor
    python3 -I sandbox/driver/sandbox.py run sandbox/scenarios/s01-einzelner-pr.json --repo queue-lab
    python3 -I sandbox/driver/sandbox.py record <lauf-verzeichnis>     # Aufzeichnung wiederholen
    python3 -I sandbox/driver/sandbox.py reset --repo queue-lab        # offene Szenario-PRs schließen
    python3 -I sandbox/driver/sandbox.py token driver                  # Installations-Token ausgeben

Akteure: „driver“ ist die Driver-App (Bot), „human“ ist das mit gh angemeldete Konto.
Aufzeichnungen landen in .sandbox-runs/ (nicht eingecheckt): Szenario, Zeitachse je PR,
Queue-PRs, Checks und die Commits auf main. Das ist das beobachtete Verhalten, gegen das
Schlange später verglichen wird.

Szenario-Format (JSON), Schritte der Reihe nach:
  {"open": "a", "flags": ["fail-full"], "files": {...}, "draft": false, "datenschutz": true,
   "ki_review": "gruen"|"rot", "as": "driver"|"human", "conflict": true, "title": "..."}
  {"push": "a", "flags": [...], "files": {...}}        neuer Commit auf den PR-Branch
  {"comment": "a", "text": "@mergifyio queue", "as": "human"}
  {"label": "a", "add": "halt"} / {"label": "a", "remove": "halt"}
  {"ready": "a"}  /  {"draft": "a"}                    Entwurfsstatus ändern
  {"edit_body": "a", "ki_review": "rot"}               Beschreibung neu setzen
  {"close": "a"}
  {"main_commit": {"pfad": "inhalt"}}                  Base bewegt sich außerhalb der Queue
  {"sleep": 30}
  {"wait": "merged"|"closed"|"done", "prs": ["a"], "timeout": 2400}
  {"wait": "quiet", "seconds": 300, "timeout": 2400}   keine Aktivität auf den PRs
Flags: siehe fixture/ci/run.sh. "conflict": true ändert shared/conflict.txt (Textkonflikt).
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
DEFAULT_ORG = os.environ.get("SANDBOX_ORG", "schlange-sandbox")


def die(msg):
    print(f"Fehler: {msg}", file=sys.stderr)
    sys.exit(1)


def log(msg):
    print(f"[{datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- Zugang -----------------------------------------------------------------------------

def app_meta(role):
    f = CONFIG_DIR / "apps" / f"{role}.json"
    if not f.exists():
        die(f"App „{role}“ ist nicht registriert (sandbox/setup.sh apps)")
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
                raise RuntimeError(f"{self.label}: {method} {path} → {e.code} {text[:300]}") from None
        raise RuntimeError(f"{self.label}: {method} {path} scheitert wiederholt")

    def paged(self, path):
        sep = "&" if "?" in path else "?"
        page, out = 1, []
        while True:
            items = self.call("GET", f"{path}{sep}per_page=100&page={page}")
            if isinstance(items, dict):  # z. B. check-runs: {"check_runs": [...]}
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
            die(f"App „{role}“ ist bei {owner} nicht installiert: {meta['html_url']}/installations/new")
        tok = jwt.call("POST", f"/app/installations/{inst['id']}/access_tokens")
        return tok["token"], 3000

    return Client(fetch, role)


def secret(env, filename):
    """Secret aus der Umgebung (CI) oder aus ~/.config/schlange-sandbox/<datei> (lokal)."""
    if os.environ.get(env):
        return os.environ[env].strip()
    f = CONFIG_DIR / filename
    return f.read_text().strip() if f.exists() else None


def human_client():
    # Fine-grained Token von karma-works, nur für Repos von schlange-sandbox gültig.
    # Rückfall auf den gh-Login nur lokal und mit Warnung: der darf mehr als die Sandbox.
    def fetch():
        tok = secret("SANDBOX_HUMAN_TOKEN", "human-token")
        if not tok:
            print("Warnung: kein Sandbox-Token (human-token), nutze den gh-Login", file=sys.stderr)
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


# --- Szenario ---------------------------------------------------------------------------

def pr_body(spec, run_id, key):
    parts = [f"Sandbox-Szenario `{run_id}`, PR `{key}`."]
    if spec.get("datenschutz", True):
        parts.append("## Datenschutz\n\nKeine personenbezogenen Daten (Testdaten).")
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
        self.started = now_iso()

    def actor(self, spec):
        return self.actors[spec.get("as", "driver")]

    def flag_path(self, key):
        return f"ci/flags/{self.run_id}-{key}"

    def flag_lines(self, flags):
        out = []
        for f in flags:
            # conflicts-with <key> verweist auf die Flag-Datei des anderen Szenario-PRs.
            if f.startswith("conflicts-with "):
                out.append(f"conflicts-with {self.run_id}-{f.split(' ', 1)[1]}")
            else:
                out.append(f)
        return "\n".join(out) + "\n"

    def commit_files(self, client, branch, files, msg):
        """Ein Commit mit allen Dateien auf den Branch (ein Push, ein synchronize-Event)."""
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
        files = dict(spec.get("files", {}))
        # Jeder Commit ändert die Flag-Datei, damit CI immer etwas zum Auswerten hat.
        files[self.flag_path(key)] = f"# {self.run_id} {key} commit {n}\n" + self.flag_lines(spec.get("flags", []))
        if spec.get("conflict"):
            files["shared/conflict.txt"] = f"Gemeinsame Datei für Textkonflikte zwischen Szenario-PRs.\nzeile: {key} ({self.run_id})\n"
        self.commit_files(client, branch, files, f"Szenario {self.run_id}: {key} ({n})")

    def step_open(self, s):
        key, client = s["open"], self.actor(s)
        branch = f"sbx/{self.run_id}/{key}"
        main = client.call("GET", f"{self.r}/git/ref/heads/main")
        client.call("POST", f"{self.r}/git/refs", {"ref": f"refs/heads/{branch}", "sha": main["object"]["sha"]})
        self.write_changes(client, branch, key, s, 1)
        pr = client.call("POST", f"{self.r}/pulls", {
            "title": s.get("title", f"Szenario {self.scenario['id']}: {key}"),
            "head": branch, "base": "main", "body": pr_body(s, self.run_id, key), "draft": s.get("draft", False),
        })
        self.prs[key] = {"number": pr["number"], "branch": branch, "commits": 1, "node_id": pr["node_id"]}
        log(f"PR {key} = #{pr['number']} ({s.get('as', 'driver')})")

    def step_push(self, s):
        key = s["push"]
        p = self.prs[key]
        p["commits"] += 1
        self.write_changes(self.actor(s), p["branch"], key, s, p["commits"])
        log(f"Push auf {key} (#{p['number']})")

    def step_comment(self, s):
        n = self.prs[s["comment"]]["number"]
        self.actor(s).call("POST", f"{self.r}/issues/{n}/comments", {"body": s["text"]})
        log(f"Kommentar auf #{n} als {s.get('as', 'driver')}: {s['text']}")

    def step_label(self, s):
        n = self.prs[s["label"]]["number"]
        c = self.actor(s)
        if "add" in s:
            c.call("POST", f"{self.r}/issues/{n}/labels", {"labels": [s["add"]]})
        else:
            c.call("DELETE", f"{self.r}/issues/{n}/labels/{s['remove']}", ok404=True)
        log(f"Label auf #{n}: {s}")

    def graphql(self, client, query, variables):
        res = client.call("POST", "/graphql", {"query": query, "variables": variables})
        if res.get("errors"):
            raise RuntimeError(res["errors"])

    def step_ready(self, s):
        p = self.prs[s["ready"]]
        self.graphql(self.actor(s), "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){clientMutationId}}", {"id": p["node_id"]})
        log(f"#{p['number']} bereit")

    def step_draft(self, s):
        p = self.prs[s["draft"]]
        self.graphql(self.actor(s), "mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){clientMutationId}}", {"id": p["node_id"]})
        log(f"#{p['number']} wieder Entwurf")

    def step_edit_body(self, s):
        key = s["edit_body"]
        n = self.prs[key]["number"]
        self.actor(s).call("PATCH", f"{self.r}/pulls/{n}", {"body": pr_body(s, self.run_id, key)})
        log(f"Beschreibung von #{n} geändert")

    def step_close(self, s):
        n = self.prs[s["close"]]["number"]
        self.actor(s).call("PATCH", f"{self.r}/pulls/{n}", {"state": "closed"})
        log(f"#{n} geschlossen")

    def step_main_commit(self, s):
        files = {path: content.replace("{run}", self.run_id) for path, content in s["main_commit"].items()}
        self.commit_files(self.actors["human"], "main", files, f"Szenario {self.run_id}: Base bewegt")
        log("Commit direkt auf main (Admin-Ausnahme)")

    def pr_state(self, key):
        pr = self.actors["human"].call("GET", f"{self.r}/pulls/{self.prs[key]['number']}")
        return "merged" if pr["merged"] else pr["state"], pr["updated_at"]

    def step_wait(self, s):
        what, deadline = s["wait"], time.time() + s.get("timeout", 2400)
        keys = s.get("prs", list(self.prs))
        last_change, seen = time.time(), {}
        while time.time() < deadline:
            states = {k: self.pr_state(k) for k in keys}
            if states != seen:
                seen, last_change = states, time.time()
                log("Stand: " + ", ".join(f"{k}=#{self.prs[k]['number']}:{v[0]}" for k, v in states.items()))
            st = {k: v[0] for k, v in states.items()}
            if what == "merged" and all(v == "merged" for v in st.values()):
                return
            if what == "closed" and all(v == "closed" for v in st.values()):
                return
            if what == "done" and all(v in ("merged", "closed") for v in st.values()):
                return
            if what == "quiet" and time.time() - last_change >= s.get("seconds", 300):
                return
            time.sleep(20)
        log(f"Zeitlimit beim Warten auf {what}; Aufzeichnung trotzdem")

    def execute(self):
        (self.out / "scenario.json").write_text(json.dumps(self.scenario, indent=2, ensure_ascii=False))
        for s in self.scenario["steps"]:
            kind = next(k for k in ("open", "push", "comment", "label", "ready", "draft", "edit_body",
                                    "close", "main_commit", "sleep", "wait") if k in s)
            if kind == "sleep":
                time.sleep(s["sleep"])
            else:
                getattr(self, f"step_{kind}")(s)
            self.save_state()
        self.save_state(finished=now_iso())

    def save_state(self, **extra):
        state = {"org": self.org, "repo": self.repo, "run_id": self.run_id, "started": self.started,
                 "prs": self.prs, **extra}
        (self.out / "run.json").write_text(json.dumps(state, indent=2))


# --- Aufzeichnung -----------------------------------------------------------------------

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
    # Aktivitätslog von Mergify (nur queue-lab): strukturierte Gründe fürs Ein- und Austragen.
    m = mergify_client() if state["repo"] == "queue-lab" else None
    if m:
        url = (f"{MERGIFY_API}/repos/{state['org']}/{state['repo']}/logs?per_page=100"
               f"&received_from={state['started']}")
        events, cursor = [], None
        for _ in range(50):
            page = m.call("GET", url + (f"&cursor={cursor}" if cursor else ""))
            events += page.get("events", [])
            cursor = page.get("next_cursor") or (page.get("links") or {}).get("next")
            if not cursor or not page.get("events"):
                break
        trace["mergify_events"] = events
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
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    log(f"Aufzeichnung: {out}")


# --- Ergebnis und Vergleich -------------------------------------------------------------
# outcome.json ist ein eigenes, textfreies Format (keine Mergify-Texte, Clean-Room) und kann
# als Sollwert eingecheckt werden: sandbox/baselines/<szenario>.json.

def write_outcome(out):
    trace = json.loads((out / "trace.json").read_text())
    key_of = {v["number"]: k for k, v in trace["prs"].items()}
    events = trace.get("mergify_events")
    prs = {}
    for k, v in trace["prs"].items():
        n = v["number"]
        mine = [e for e in events or [] if e.get("pull_request") == n]
        entered = [e for e in mine if "queue" in e["type"] and e["type"].endswith("enter")]
        left = [e for e in mine if "queue" in e["type"] and e["type"].endswith("leave")]
        prs[k] = {
            "end": "merged" if v["merged"] else v["state"],
            # Ohne Mergify-Log (schlange-e2e) liefert später Schlanges eigenes Ereignisprotokoll diese Felder.
            "queued": bool(entered) if events is not None else None,
            "enter_count": len(entered) if events is not None else None,
            "dequeue_codes": [(e.get("metadata") or {}).get("dequeue_code") for e in left
                              if not (e.get("metadata") or {}).get("merged")] if events is not None else None,
        }
    landed = []
    for cm in reversed(trace["main"]):  # älteste zuerst
        for num in re.findall(r"\(#(\d+)\)", cm["message"].split("\n")[0]):
            if int(num) in key_of:
                landed.append(key_of[int(num)])
    batches = []
    for q in reversed(trace["queue_prs"]):
        keys = sorted({key_of[int(x)] for x in re.findall(r"#(\d+)", q.get("body") or "") if int(x) in key_of})
        ci = next((x["conclusion"] for x in q["checks"] if x["name"] == "ci-ok"), None)
        batches.append({"prs": keys, "ci_ok": ci})
    outcome = {"scenario": trace["run"]["run_id"].split("-")[0].upper(), "repo": trace["run"]["repo"],
               "prs": prs, "landed": landed, "batches": batches}
    (out / "outcome.json").write_text(json.dumps(outcome, indent=2, ensure_ascii=False) + "\n")
    return outcome


def compare_outcomes(expected_path, actual_path, strict_batches=False):
    """0 = gleiches Verhalten, 1 = Abweichungen (werden ausgegeben)."""
    exp = json.loads(Path(expected_path).read_text())
    act = json.loads(Path(actual_path).read_text())
    diffs = []
    for k in sorted(set(exp["prs"]) | set(act["prs"])):
        e, a = exp["prs"].get(k, {}), act["prs"].get(k, {})
        for field in ("end", "queued", "dequeue_codes"):
            # None heißt: diese Seite kann das Feld (noch) nicht beobachten.
            if e.get(field) is not None and a.get(field) is not None and e.get(field) != a.get(field):
                diffs.append(f"PR {k}.{field}: erwartet {e.get(field)}, beobachtet {a.get(field)}")
    if exp["landed"] != act["landed"]:
        diffs.append(f"Landereihenfolge: erwartet {exp['landed']}, beobachtet {act['landed']}")
    # Bündel hängen vom Timing ab (batch_max_wait_time); nur auf Wunsch streng vergleichen.
    if strict_batches and [b["prs"] for b in exp["batches"]] != [b["prs"] for b in act["batches"]]:
        diffs.append(f"Bündel: erwartet {exp['batches']}, beobachtet {act['batches']}")
    if diffs:
        print("ABWEICHUNG\n  " + "\n  ".join(diffs))
        return 1
    print("GLEICH")
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


# --- Befehle ----------------------------------------------------------------------------

def busy(org, repo):
    """Offene Szenario- oder Queue-PRs: dann läuft dort gerade etwas (lokal, CI oder anderer Agent)."""
    open_prs = human_client().paged(f"/repos/{org}/{repo}/pulls?state=open")
    return [p["number"] for p in open_prs if p["head"]["ref"].startswith(("sbx/", QUEUE_PREFIX))]


def wait_idle(org, repo, timeout):
    # Zwei Szenarien gleichzeitig in einem Repo landen im selben Bündel und verfälschen sich.
    deadline = time.time() + timeout
    while (prs := busy(org, repo)):
        if time.time() > deadline:
            die(f"{org}/{repo} ist belegt (offene PRs {prs}); aufräumen mit: sandbox.py reset --repo {repo}")
        log(f"{repo} belegt (offene PRs {prs}), warte …")
        time.sleep(30)


def cmd_run(a):
    scenario = json.loads(Path(a.scenario).read_text())
    if not a.no_wait:
        wait_idle(a.org, a.repo, a.idle_timeout)
    run_id = f"{scenario['id'].lower()}-{a.repo}-{datetime.now().strftime('%Y%m%d-%H%M%S')}"
    out = RUNS / run_id
    out.mkdir(parents=True)
    log(f"Szenario {scenario['id']} – {scenario['title']} in {a.org}/{a.repo} ({out})")
    try:
        Run(a.org, a.repo, scenario, out).execute()
    finally:
        record(out)
    write_outcome(out)
    if a.baseline:
        sys.exit(compare_outcomes(Path(a.baseline), out / "outcome.json"))


def cmd_reset(a):
    c = installation_client("driver", a.org)
    h = human_client()
    r = f"/repos/{a.org}/{a.repo}"
    for pr in c.paged(f"{r}/pulls?state=open"):
        if pr["head"]["ref"].startswith("sbx/"):
            c.call("PATCH", f"{r}/pulls/{pr['number']}", {"state": "closed"})
            log(f"#{pr['number']} geschlossen")
    for ref in h.paged(f"{r}/git/matching-refs/heads/sbx/"):
        h.call("DELETE", f"{r}/git/{ref['ref']}", ok404=True)
        log(f"{ref['ref']} gelöscht")


def cmd_doctor(a):
    ok = True
    for role in ("queue", "driver"):
        f = CONFIG_DIR / "apps" / f"{role}.json"
        if not f.exists():
            print(f"☐ App {role} nicht registriert")
            ok = False
            continue
        try:
            jwt = Client(lambda r=role: (app_jwt(r), 500), role)
            inst = jwt.call("GET", f"/orgs/{a.org}/installation", ok404=True)
            if not inst:
                print(f"☐ App {role} nicht in {a.org} installiert: {app_meta(role)['html_url']}/installations/new")
                ok = False
                continue
            tok = jwt.call("POST", f"/app/installations/{inst['id']}/access_tokens")["token"]
            repos = Client(lambda t=tok: (t, 3000), role).call("GET", "/installation/repositories")
            names = sorted(x["name"] for x in repos["repositories"])
            print(f"→ App {role} installiert, Repos: {', '.join(names) or '–'}")
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
    r.add_argument("--baseline", help="outcome.json als Sollwert; Exit 1 bei Abweichung")
    r.add_argument("--no-wait", action="store_true", help="nicht warten, bis das Repo frei ist")
    r.add_argument("--idle-timeout", type=int, default=1800)
    sub.add_parser("record").add_argument("dir")
    sub.add_parser("outcome").add_argument("dir")
    c = sub.add_parser("compare")
    c.add_argument("expected")
    c.add_argument("actual")
    c.add_argument("--strict-batches", action="store_true")
    sub.add_parser("reset").add_argument("--repo", required=True)
    sub.add_parser("doctor")
    sub.add_parser("token").add_argument("role", choices=["queue", "driver", "shadow"])
    # --org auch nach dem Unterbefehl erlauben
    for sp in sub.choices.values():
        sp.add_argument("--org", default=argparse.SUPPRESS)
    a = p.parse_args()
    {"run": cmd_run, "record": lambda a: record(Path(a.dir)), "reset": cmd_reset,
     "outcome": lambda a: print(json.dumps(write_outcome(Path(a.dir)), indent=2, ensure_ascii=False)),
     "compare": lambda a: sys.exit(compare_outcomes(a.expected, a.actual, a.strict_batches)),
     "doctor": cmd_doctor, "token": cmd_token}[a.cmd](a)


if __name__ == "__main__":
    main()
