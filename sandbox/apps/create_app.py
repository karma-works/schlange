#!/usr/bin/env python3
"""Registers a GitHub App through the manifest flow.

The browser needs only one click ("Create GitHub App"). The script then exchanges the
one-time code for the app ID and private key and stores both in
~/.config/schlange-sandbox/apps/ (directory 0700, keys 0600). None of it ends up in the
repo.

    python3 -I sandbox/apps/create_app.py queue  --org schlange-sandbox --webhook-url https://smee.io/…
    python3 -I sandbox/apps/create_app.py driver --org schlange-sandbox
    python3 -I sandbox/apps/create_app.py shadow --user karma-works   # shadow mode on Arztagent

Standard library only. An existing entry is not overwritten (--force).
"""

import argparse
import html
import json
import os
import secrets
import sys
import threading
import urllib.request
import webbrowser
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

HERE = Path(__file__).resolve().parent
CONFIG_DIR = Path(os.environ.get("SCHLANGE_SANDBOX_CONFIG", Path.home() / ".config/schlange-sandbox"))
HOMEPAGE = "https://github.com/karma-works/schlange"
# Placeholder while there is no receiver yet; the webhook then stays inactive.
NO_WEBHOOK = "https://example.invalid/schlange-webhook"


def die(msg):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(1)


def build_manifest(role, args, redirect_url):
    template = json.loads((HERE / "manifests" / f"{role}.json").read_text())
    wants_webhook = template.pop("webhook")
    manifest = {
        **template,
        "url": HOMEPAGE,
        "redirect_url": redirect_url,
        "hook_attributes": {
            "url": args.webhook_url or NO_WEBHOOK,
            "active": bool(wants_webhook and args.webhook_url),
        },
    }
    if args.name:
        manifest["name"] = args.name
    return manifest


def convert(code):
    req = urllib.request.Request(
        f"https://api.github.com/app-manifests/{code}/conversions",
        method="POST",
        headers={"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"},
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def write_secret(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(value)


def store(role, app, owner, owner_type):
    apps = CONFIG_DIR / "apps"
    apps.mkdir(parents=True, exist_ok=True)
    os.chmod(CONFIG_DIR, 0o700)
    os.chmod(apps, 0o700)
    write_secret(apps / f"{role}.pem", app["pem"])
    if app.get("webhook_secret"):
        write_secret(apps / f"{role}.webhook-secret", app["webhook_secret"])
    meta = {
        "role": role,
        "id": app["id"],
        "slug": app["slug"],
        "client_id": app["client_id"],
        "name": app["name"],
        "html_url": app["html_url"],
        "bot_login": f"{app['slug']}[bot]",
        "owner": owner,
        "owner_type": owner_type,
    }
    (apps / f"{role}.json").write_text(json.dumps(meta, indent=2) + "\n")
    return meta


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("role", choices=["queue", "driver", "shadow"])
    who = p.add_mutually_exclusive_group(required=True)
    who.add_argument("--org", help="organisation that owns the app")
    who.add_argument("--user", help="personal account that owns the app")
    p.add_argument("--webhook-url", help="webhook receiver (e.g. a smee.io channel); without it the webhook stays inactive")
    p.add_argument("--name", help="different app name, if the name is already taken on GitHub")
    p.add_argument("--force", action="store_true", help="replace existing credentials")
    p.add_argument("--no-browser", action="store_true", help="only print the URL")
    args = p.parse_args()

    if (CONFIG_DIR / "apps" / f"{args.role}.json").exists() and not args.force:
        meta = json.loads((CONFIG_DIR / "apps" / f"{args.role}.json").read_text())
        print(f"App \"{args.role}\" is already registered: {meta['html_url']} (replace with --force)")
        return

    owner = args.org or args.user
    owner_type = "org" if args.org else "user"
    new_url = (
        f"https://github.com/organizations/{owner}/settings/apps/new"
        if args.org
        else "https://github.com/settings/apps/new"
    )
    state = secrets.token_urlsafe(24)
    result = {}
    done = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, status, body):
            data = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            url = urlparse(self.path)
            if url.path == "/":
                manifest = build_manifest(args.role, args, f"http://127.0.0.1:{port}/callback")
                self.send(200, f"""<!doctype html><meta charset="utf-8"><title>Register app</title>
<body style="font-family:system-ui;max-width:40em;margin:3em auto">
<h1>{html.escape(manifest['name'])}</h1>
<p>Continue to GitHub. Only click "Create GitHub App" there.</p>
<form id="f" method="post" action="{html.escape(new_url)}?state={state}">
<input type="hidden" name="manifest" value="{html.escape(json.dumps(manifest))}">
<button>Continue to GitHub</button></form>
<script>document.getElementById('f').submit()</script>""")
                return
            if url.path == "/callback":
                q = parse_qs(url.query)
                if q.get("state", [""])[0] != state or "code" not in q:
                    self.send(400, "Invalid callback (state does not match).")
                    return
                try:
                    result["meta"] = store(args.role, convert(q["code"][0]), owner, owner_type)
                except Exception as e:  # noqa: BLE001 – report the error to browser and terminal
                    result["error"] = str(e)
                    self.send(500, f"Conversion failed: {html.escape(str(e))}")
                    done.set()
                    return
                install = result["meta"]["html_url"] + "/installations/new"
                self.send(200, f"""<!doctype html><meta charset="utf-8"><title>Done</title>
<body style="font-family:system-ui;max-width:40em;margin:3em auto">
<h1>App registered</h1><p>The credentials are stored locally. Next step: install the app.</p>
<p><a href="{html.escape(install)}">{html.escape(install)}</a></p>""")
                done.set()
                return
            self.send(404, "not found")

    server = HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    start = f"http://127.0.0.1:{port}/"
    print(f"Registering \"{args.role}\" for {owner}: {start}", flush=True)
    if not args.no_browser:
        webbrowser.open(start)
    if not done.wait(timeout=900):
        server.shutdown()
        die("No callback from GitHub within 15 minutes.")
    server.shutdown()
    if "error" in result:
        die(result["error"])
    meta = result["meta"]
    print(f"Registered: {meta['name']} (ID {meta['id']}, bot {meta['bot_login']})")
    print(f"Install now: {meta['html_url']}/installations/new")


if __name__ == "__main__":
    main()
