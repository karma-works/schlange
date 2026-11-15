"""Offline operational regressions; no GitHub or Docker daemon required."""
import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BackupTests(unittest.TestCase):
    def test_online_backup_includes_committed_wal_and_is_private(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source = directory / "schlange.db"
            with sqlite3.connect(source) as writer:
                writer.execute("PRAGMA journal_mode=WAL")
                writer.execute("CREATE TABLE inbox (editor TEXT)")
                writer.execute("INSERT INTO inbox VALUES ('original-editor')")
                writer.commit()
                # Keep the writer open: copying the main file alone would lose the row.
                destination = load("backup").backup(source, directory / "backups")
                with sqlite3.connect(destination) as restored:
                    self.assertEqual(restored.execute("SELECT * FROM inbox").fetchall(),
                                     [("original-editor",)])
                    self.assertEqual(restored.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                self.assertEqual(destination.stat().st_mode & 0o777, 0o600)
                self.assertEqual(destination.parent.stat().st_mode & 0o777, 0o700)

    def test_missing_source_never_creates_empty_database(self):
        backup = load("backup").backup
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with self.assertRaises((OSError, sqlite3.Error)):
                backup(directory / "missing.db", directory / "backups")
            self.assertFalse((directory / "missing.db").exists())
            self.assertEqual(list(directory.glob("backups/*.db")), [])


class MonitorTests(unittest.TestCase):
    def test_halt_config_error_identity_hold_and_missing_repo_alert(self):
        monitor = load("monitor")
        healthy = {"demo/repo": {"halted": {}, "config_error": None,
                                 "state": {"check_identity_unavailable": []}, "role": "shadow"}}
        self.assertEqual(monitor.problems(healthy, ["demo/repo"]), [])
        self.assertTrue(monitor.problems({}, ["demo/repo"]))
        for key, value in [("halted", {"main": "tree mismatch"}),
                           ("config_error", "invalid configuration"),
                           ("state", {"check_identity_unavailable": ["main"]}),
                           ("role", "follower")]:
            worker = dict(healthy["demo/repo"], **{key: value})
            self.assertTrue(monitor.problems({"demo/repo": worker}, ["demo/repo"]), key)

    def test_admin_token_is_header_and_health_is_separate(self):
        import json
        import threading
        from http.server import BaseHTTPRequestHandler, HTTPServer
        seen = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append((self.path, self.headers.get("Authorization")))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(json.dumps({"demo/repo": {"role": "leader"}}).encode())

            def log_message(self, *args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            load("monitor").check(f"http://127.0.0.1:{server.server_port}",
                                  "private-token", ["demo/repo"])
            self.assertEqual(seen, [("/healthz", None), ("/readyz", None),
                                    ("/status", "Bearer private-token")])
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
