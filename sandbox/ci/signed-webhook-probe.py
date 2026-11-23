#!/usr/bin/env python3
"""One Actions-only signed Smee delivery probe for the queue-lab sandbox.

Raw relay data stays in memory. The public report contains only bounded structural results.
This is not a GitHub redelivery or follower/503 test.
"""

import argparse
import hashlib
import hmac
import http.client
import importlib.util
import json
import os
import re
import select
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DRIVER_PATH = ROOT / "sandbox/driver/sandbox.py"
START = ROOT / "sandbox/ci/start-schlange.sh"
MAX_RELAY_EVENT = 2_000_000
MAX_RELAY_LINE = 256_000
ALLOWED_REPO = "schlange-sandbox/queue-lab"

spec = importlib.util.spec_from_file_location("sandbox_driver_for_ingress", DRIVER_PATH)
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class ProbeError(Exception):
    pass


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


NO_REDIRECT = urllib.request.build_opener(_NoRedirect)


@dataclass(repr=False)
class SelectedEvent:
    body: bytes = field(repr=False)
    signature: str = field(repr=False)
    delivery: str = field(repr=False)
    event: str
    representation: str

    def __repr__(self):
        return f"SelectedEvent(event={self.event!r}, representation={self.representation!r})"


def _headers(value):
    if not isinstance(value, dict) or len(value) > 64:
        raise ProbeError("headers_invalid")
    out = {}
    for key, item in value.items():
        if not isinstance(key, str) or not isinstance(item, str):
            raise ProbeError("headers_invalid")
        lowered = key.lower()
        if lowered in out or len(lowered) > 100 or len(item) > 500:
            raise ProbeError("headers_invalid")
        out[lowered] = item
    signature = out.get("x-hub-signature-256")
    delivery = out.get("x-github-delivery")
    event = out.get("x-github-event")
    content_type = out.get("content-type", "").split(";", 1)[0].strip().lower()
    if (not isinstance(signature, str) or not re.fullmatch(r"sha256=[0-9a-f]{64}", signature)
            or not isinstance(delivery, str)
            or not re.fullmatch(r"[0-9A-Za-z-]{1,100}", delivery)
            or event != "pull_request" or content_type != "application/json"):
        raise ProbeError("headers_invalid")
    return signature, delivery, event


def _signed_body(body, signature, secret):
    candidates = []
    if isinstance(body, str):
        candidates.append(("string", body.encode("utf-8")))
    elif isinstance(body, (dict, list)):
        try:
            encoded = json.dumps(
                body, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        except (RecursionError, TypeError, ValueError):
            raise ProbeError("relay_event_invalid") from None
        candidates.append(("json_compact", encoded))
    else:
        raise ProbeError("relay_event_invalid")
    expected = signature.removeprefix("sha256=")
    for representation, candidate in candidates:
        if len(candidate) > MAX_RELAY_EVENT:
            raise ProbeError("relay_event_invalid")
        digest = hmac.new(secret, candidate, hashlib.sha256).hexdigest()
        if hmac.compare_digest(digest, expected):
            return representation, candidate
    raise ProbeError("representation_unverified")


def _scope_matches(payload, repository, branch, number, driver_login):
    pull = payload.get("pull_request") if isinstance(payload, dict) else None
    repository_value = payload.get("repository") if isinstance(payload, dict) else None
    head = pull.get("head") if isinstance(pull, dict) else None
    base = pull.get("base") if isinstance(pull, dict) else None
    user = pull.get("user") if isinstance(pull, dict) else None
    sender = payload.get("sender") if isinstance(payload, dict) else None
    return (isinstance(pull, dict) and type(number) is int
            and payload.get("action") == "opened"
            and type(payload.get("number")) is int and payload.get("number") == number
            and type(pull.get("number")) is int and pull.get("number") == number
            and pull.get("draft") is True
            and isinstance(repository_value, dict)
            and repository_value.get("full_name") == repository
            and isinstance(head, dict) and head.get("ref") == branch
            and isinstance(base, dict) and base.get("ref") == "main"
            and isinstance(user, dict) and user.get("login") == driver_login
            and isinstance(sender, dict) and sender.get("login") == driver_login)


def matches_target_untrusted(event, repository, branch, number, driver_login):
    """Cheap selection only; selected bytes are independently verified and reparsed."""
    if not isinstance(event, dict) or event.get("x-github-event") != "pull_request":
        return False
    body = event.get("body")
    if isinstance(body, dict):
        payload = body
    elif isinstance(body, str) and len(body.encode("utf-8")) <= MAX_RELAY_EVENT:
        try:
            payload = json.loads(body)
        except (RecursionError, UnicodeError, ValueError):
            return False
    else:
        return False
    return _scope_matches(payload, repository, branch, number, driver_login)


def select_signed_event(event, secret, repository, branch, number, driver_login):
    if not isinstance(event, dict) or "body" not in event:
        raise ProbeError("relay_event_invalid")
    query = event.get("query", {})
    if not isinstance(query, dict) or len(query) > 32:
        raise ProbeError("relay_event_invalid")
    signature, delivery, kind = _headers({
        key: value for key, value in event.items() if key not in {"body", "query"}
    })
    representation, body = _signed_body(event.get("body"), signature, secret)
    try:
        payload = json.loads(body)
    except (RecursionError, UnicodeError, ValueError):
        raise ProbeError("relay_event_invalid") from None
    if not _scope_matches(payload, repository, branch, number, driver_login):
        raise ProbeError("event_scope_invalid")
    return SelectedEvent(body, signature, delivery, kind, representation)


def parse_sse_data(lines):
    data = []
    total = 0
    for raw in lines:
        if not isinstance(raw, bytes) or len(raw) > MAX_RELAY_LINE:
            raise ProbeError("relay_event_invalid")
        total += len(raw)
        if total > MAX_RELAY_EVENT:
            raise ProbeError("relay_event_invalid")
        line = raw.rstrip(b"\r\n")
        if not line:
            break
        if line.startswith(b"data:"):
            value = line[5:]
            if value.startswith(b" "):
                value = value[1:]
            data.append(value)
        elif line.startswith((b":", b"event:", b"id:", b"retry:")):
            continue
        else:
            raise ProbeError("relay_event_invalid")
    if not data:
        raise ProbeError("relay_event_invalid")
    try:
        event = json.loads(b"\n".join(data))
    except (RecursionError, UnicodeError, ValueError):
        raise ProbeError("relay_event_invalid") from None
    if not isinstance(event, dict):
        raise ProbeError("relay_event_invalid")
    return event


def _next_sse(response):
    lines = []
    total = 0
    while True:
        line = response.readline(MAX_RELAY_LINE + 1)
        if not line:
            raise ProbeError("relay_closed")
        if len(line) > MAX_RELAY_LINE:
            raise ProbeError("relay_event_invalid")
        if line in (b"\n", b"\r\n"):
            if lines:
                if any(item.startswith(b"data:") for item in lines):
                    return parse_sse_data([*lines, line])
                lines.clear()
                total = 0
            continue
        total += len(line)
        if total > MAX_RELAY_EVENT:
            raise ProbeError("relay_event_invalid")
        lines.append(line)


def _relay_url(value):
    parsed = urllib.parse.urlsplit(value or "")
    if (parsed.scheme != "https" or parsed.hostname != "smee.io" or parsed.port is not None
            or parsed.username is not None or parsed.password is not None
            or parsed.query or parsed.fragment
            or not re.fullmatch(r"/[A-Za-z0-9_-]{8,200}", parsed.path)):
        raise ProbeError("relay_configuration_invalid")
    return parsed.geturl()


def _configured_relay_url(value):
    if not value:
        value = driver.app_hook_url("queue")
    return _relay_url(value)


def _post_loopback(port, selected):
    if type(port) is not int or not 1 <= port <= 65535:
        raise ProbeError("loopback_invalid")
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        connection.request("POST", "/webhook", body=selected.body, headers={
            "Content-Type": "application/json",
            "X-GitHub-Event": selected.event,
            "X-GitHub-Delivery": selected.delivery,
            "X-Hub-Signature-256": selected.signature,
        })
        response = connection.getresponse()
        response.read(4097)
        return response.status
    except (OSError, http.client.HTTPException):
        raise ProbeError("loopback_failed") from None
    finally:
        connection.close()


def delivery_shape(database, repository):
    try:
        uri = Path(database).resolve().as_uri() + "?mode=ro"
        db = sqlite3.connect(uri, uri=True)
        inbox = db.execute(
            "SELECT COUNT(*), SUM(CASE WHEN processed = 0 THEN 1 ELSE 0 END) "
            "FROM inbox WHERE repo = ?", (repository,)).fetchone()
        deliveries = db.execute(
            "SELECT COUNT(*) FROM deliveries WHERE guid IN "
            "(SELECT guid FROM inbox WHERE repo = ?)", (repository,)).fetchone()[0]
        db.close()
    except (OSError, sqlite3.Error, TypeError, ValueError):
        raise ProbeError("store_invalid") from None
    values = (inbox[0], inbox[1] or 0, deliveries)
    if any(type(value) is not int or value < 0 for value in values):
        raise ProbeError("store_invalid")
    return {"inbox_rows": values[0], "pending_rows": values[1],
            "delivery_rows": values[2]}


def _wait_delivery(database, repository, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            shape = delivery_shape(database, repository)
            if shape == {"inbox_rows": 1, "pending_rows": 0, "delivery_rows": 1}:
                return shape
        except ProbeError:
            pass
        time.sleep(0.25)
    raise ProbeError("inbox_timeout")


def _process_running(pid, proc_root=Path("/proc")):
    try:
        fields = (proc_root / str(pid) / "stat").read_text().split()
    except FileNotFoundError:
        return False
    except OSError:
        return True
    return len(fields) < 3 or fields[2] != "Z"


def _option(command, name):
    positions = [index for index, value in enumerate(command) if value == name]
    if len(positions) != 1 or positions[0] + 1 >= len(command):
        raise ProbeError("service_identity_invalid")
    return command[positions[0] + 1]


def _verify_service_identity(pid, binary, data_dir, proc_root=Path("/proc")):
    process = proc_root / str(pid)
    try:
        executable = (process / "exe").resolve(strict=True)
        raw = (process / "cmdline").read_bytes()
        command = [os.fsdecode(value) for value in raw.split(b"\0") if value]
        expected_binary = Path(binary).resolve(strict=True)
        expected_data = Path(data_dir).resolve(strict=True)
    except (OSError, ValueError):
        raise ProbeError("service_identity_invalid") from None
    if (len(raw) > 16_384 or len(command) > 64 or executable != expected_binary
            or len(command) < 2 or command[1] != "serve"
            or command.count("--shadow") != 1 or command.count("--no-lease") != 1
            or _option(command, "--repo") != ALLOWED_REPO
            or Path(_option(command, "--data-dir")).resolve() != expected_data
            or _option(command, "--listen") != "127.0.0.1:8080"):
        raise ProbeError("service_identity_invalid")


def _stop_service(pid_file, binary):
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise ProbeError("service_identity_invalid")
    try:
        pid = int(Path(pid_file).read_text().strip())
        pidfd = os.pidfd_open(pid)
    except (AttributeError, OSError, ValueError):
        raise ProbeError("service_identity_invalid") from None
    try:
        data_dir = Path(os.environ["RUNNER_TEMP"]) / "schlange-data"
        _verify_service_identity(pid, binary, data_dir)
        signal.pidfd_send_signal(pidfd, signal.SIGTERM)
        poller = select.poll()
        poller.register(pidfd, select.POLLIN)
        for _ in range(100):
            if poller.poll(100) or not _process_running(pid):
                return
        raise ProbeError("service_stop_timeout")
    finally:
        os.close(pidfd)


def _restart_service(binary):
    env = dict(os.environ)
    env.update({"SANDBOX_REPO": "queue-lab", "SANDBOX_MODE": "shadow", "SCHLANGE_PORT": "8080"})
    try:
        subprocess.run(["bash", str(START), str(binary)], env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=90)
    except (OSError, subprocess.SubprocessError):
        raise ProbeError("service_restart_failed") from None


def _write_report(path, report):
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary.replace(target)


def _provenance():
    repository = os.environ.get("GITHUB_REPOSITORY")
    commit = os.environ.get("GITHUB_SHA")
    run_id = os.environ.get("GITHUB_RUN_ID")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT")
    if (repository != "karma-works/schlange" or not re.fullmatch(r"[0-9a-f]{40}", commit or "")
            or not re.fullmatch(r"[1-9][0-9]*", run_id or "")
            or not re.fullmatch(r"[1-9][0-9]*", attempt or "")):
        raise ProbeError("provenance_invalid")
    return {"repository": repository, "commit": commit,
            "run_id": int(run_id), "run_attempt": int(attempt)}


def run(args):
    report = {"schema": 1, "repository": ALLOWED_REPO, "complete": False}
    run = None
    prefix = None
    try:
        report["source"] = _provenance()
        if os.environ.get("GITHUB_ACTIONS") != "true":
            raise ProbeError("provenance_invalid")
        run_key = f"ingress-{report['source']['run_id']}-{report['source']['run_attempt']}"
        prefix = f"sbx/{run_key}/"
        branch = prefix + "a"
        secret_file = Path(os.environ["SCHLANGE_SANDBOX_CONFIG"]) / "apps/queue.webhook-secret"
        if (not secret_file.is_file() or not 16 <= secret_file.stat().st_size <= 4096):
            raise ProbeError("secret_invalid")
        secret = secret_file.read_bytes()
        driver_login = driver.app_meta("driver").get("bot_login")
        if not isinstance(driver_login, str) or not re.fullmatch(r"[A-Za-z0-9-]+\[bot\]", driver_login):
            raise ProbeError("driver_identity_invalid")
        relay_url = _configured_relay_url(os.environ.get("SANDBOX_SMEE_URL"))
        request = urllib.request.Request(relay_url, headers={"Accept": "text/event-stream"})
        try:
            response = NO_REDIRECT.open(request, timeout=60)
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            raise ProbeError("relay_connection_failed") from None
        with response:
            if not (response.headers.get("Content-Type") or "").lower().startswith("text/event-stream"):
                raise ProbeError("relay_connection_invalid")
            if driver.busy(driver.ALLOWED_ORG, "queue-lab"):
                raise ProbeError("sandbox_busy")
            out = Path(os.environ["RUNNER_TEMP"]) / run_key
            run = driver.Run(driver.ALLOWED_ORG, "queue-lab", {"id": "INGRESS", "steps": []}, out)
            run.step_open({"open": "a", "draft": True})
            number = run.prs["a"]["number"]
            deadline = time.time() + args.timeout
            selected = None
            while time.time() < deadline:
                event = _next_sse(response)
                if not matches_target_untrusted(
                        event, ALLOWED_REPO, branch, number, driver_login):
                    continue
                selected = select_signed_event(
                    event, secret, ALLOWED_REPO, branch, number, driver_login)
                break
            if selected is None:
                raise ProbeError("selected_event_timeout")
        first = _post_loopback(8080, selected)
        if first != 202:
            raise ProbeError("first_delivery_not_accepted")
        database = Path(os.environ["RUNNER_TEMP"]) / "schlange-data/schlange.db"
        shape = _wait_delivery(database, ALLOWED_REPO)
        if _post_loopback(8080, selected) != 200:
            raise ProbeError("live_duplicate_not_suppressed")
        _stop_service(Path(os.environ["RUNNER_TEMP"]) / "schlange.pids", args.binary)
        _restart_service(args.binary)
        if _post_loopback(8080, selected) != 200:
            raise ProbeError("restart_duplicate_not_suppressed")
        if delivery_shape(database, ALLOWED_REPO) != shape:
            raise ProbeError("store_changed_after_duplicate")
        report.update({
            "pull_request": number,
            "signature_verified": True,
            "relay_representation": selected.representation,
            "first_delivery_accepted": True,
            "live_duplicate_suppressed": True,
            "service_restarted": True,
            "restart_duplicate_suppressed": True,
            **shape,
            "complete": True,
        })
    except (ProbeError, KeyError, OSError, RecursionError, RuntimeError, SystemExit,
            TypeError, ValueError) as error:
        report["reason"] = str(error) if isinstance(error, ProbeError) else "probe_invalid"
    finally:
        if prefix is not None:
            try:
                driver.cleanup(driver.ALLOWED_ORG, "queue-lab", prefix)
            except (RuntimeError, OSError, ValueError):
                report["complete"] = False
                report["reason"] = "cleanup_failed"
        _write_report(args.output, report)
    if not report["complete"]:
        raise SystemExit(1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--timeout", type=int, default=180)
    args = parser.parse_args()
    if not 10 <= args.timeout <= 600:
        raise SystemExit("invalid timeout")
    run(args)


if __name__ == "__main__":
    main()
