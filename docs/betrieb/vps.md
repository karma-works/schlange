# Self-hosted VPS deployment

The [deployment template](../../ops/compose.yaml) runs one Schlange instance, a Caddy TLS
ingress and an allowlisted Squid forward proxy. Start in shadow mode with a dedicated
read-only GitHub App. Active operation follows [shadow acceptance](schattenbetrieb.md) and
the [single-writer parity contract](arztagent-parity.md); installing this stack does not
perform a production cutover. Arztagent webhooks and PR metadata stay on the approved VPS.

## Prepare the host

Use a maintained Linux host with Docker Engine, Compose v2, systemd and Python 3 with SQLite
support. Restrict SSH to operators; allow inbound TCP 80/443 for ACME and webhooks. Keep Docker's
API socket private. Build the Schlange image from the reviewed commit and run its image-size
check. Review the example's immutable Caddy and Ubuntu Squid image pins (used for configuration
checks), updating them under the host's patch policy. Record all three image identifiers with
the release.

When the build host uses a private HTTPS proxy CA, pass the combined trusted CA bundle as a
BuildKit secret: `docker build --secret id=proxy_ca,src=/path/to/ca-bundle.pem -t schlange:dev .`.
The optional secret is mounted only for dependency fetching; it is never copied into an image
layer or the final runtime certificate store. Keep TLS verification enabled.

Copy `ops/` to `/etc/schlange`, then prepare directories as root:

```bash
install -d -m 0750 /etc/schlange
install -d -m 0700 /etc/schlange/secrets /var/backups/schlange /var/lib/schlange/reports
install -d -m 0700 -o 65532 -g 65532 /var/lib/schlange/data /var/lib/schlange/tls
cp /etc/schlange/stack.env.example /etc/schlange/stack.env
chmod 0600 /etc/schlange/stack.env
chmod 0644 /etc/schlange/Caddyfile /etc/schlange/squid.conf
```

Edit `stack.env`: public DNS name, certificate contact, image identifiers, App ID and repository
list. Its syntax must work both as a Compose env file and systemd `EnvironmentFile`: simple
`NAME=value` lines, no shell substitutions. For multiple repositories use a comma-separated list
without spaces. The included systemd units use `/usr/bin/docker` and `/var/lib/schlange`; adapt
those paths too if Docker or `SCHLANGE_ROOT` is elsewhere.

Install the App private key, a random webhook secret of at least 16 bytes, and a separate random
admin token into `secrets/app-key.pem`, `secrets/webhook-secret`, `secrets/admin-token`. Set each
file to owner `65532:65532`, mode `0400`; leave the parent directory root-only. Docker bind-mounts
the files individually. Secret values never appear in Compose environment variables, CLI
arguments, Git or logs. Set the GitHub App webhook URL to `https://<host>/webhook` with the same
secret. The shadow App needs `actions: read` in addition to the other documented read permissions
for check identity. Verify with `schlange doctor --shadow` before acceptance begins.

## Start and verify

```bash
cd /etc/schlange
docker compose --env-file stack.env -f compose.yaml config --quiet
install -m 0644 systemd/schlange*.service systemd/schlange*.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now schlange.service
systemctl enable --now schlange-monitor.timer schlange-backup.timer schlange-shadow-report.timer
systemctl start schlange-monitor.service schlange-backup.service schlange-shadow-report.service
```

Run the live doctor using the same file-mounted key and environment:

```bash
docker compose --env-file stack.env -f compose.yaml exec -T schlange \
  doctor --private-key /run/secrets/app-key.pem --shadow --json
```

Require zero FAILs and investigate unexpected WARNs. In active mode omit `--shadow` and set
`SCHLANGE_SHADOW=false` after the approved cutover. The container needs no external schema mount
when using the embedded supported-subset validator; an explicit schema remains an optional
operator choice.

Check `http://127.0.0.1:18080/healthz` and `/readyz` from the host, then confirm the signed GitHub
ping delivery succeeds. Public requests to `/status`, `/healthz`, `/readyz` and queue-control
paths return 404; administrative access stays on the host or an authenticated SSH tunnel.
The image's offline doctor health check checks its installation; it does not establish a live
worker heartbeat or leadership. The monitor timer covers those separately.

## Isolation and ingress

Schlange runs as `65532:65532` with a read-only root, all capabilities dropped,
`no-new-privileges`, a PID limit, CPU/memory limits and only `/data` writable. Its internal
Docker network has no external route. Its HTTP client uses the explicit `HTTPS_PROXY` setting
to Squid, which accepts only CONNECT to exactly `api.github.com:443`; arbitrary HTTP,
other ports and other hosts are denied. Squid does not intercept TLS or log requests. Caddy
has a separate internet network for ACME. The pinned Caddy binary carries the
`NET_BIND_SERVICE` file capability, so ingress retains that single bounding capability
alongside `no-new-privileges`; otherwise Linux refuses to execute it. Schlange and Squid retain
zero capabilities. Caddy's only application route is `POST /webhook`, with
a 2 MB body limit and bounded timeouts. Do not attach Schlange to an external Docker network,
publish Squid's port, or replace the ACL with a broad GitHub domain wildcard.

Caddy actively checks `/readyz` every two seconds and removes an unavailable upstream. A lease
follower can still answer `/healthz` successfully; liveness must never select the writer.
There remains a race between readiness sampling and admission. The service checks leadership
again at admission and returns 503 when it cannot accept a delivery. Caddy does not automatically
retry webhook POSTs. Alert on rejected deliveries and use GitHub's App webhook delivery page
to redeliver each failed request after `/readyz` is green. GitHub does not automatically retry
them. Preserve the delivery ID for deduplication. An accepted persistent-inbox delivery survives
a restart; independently stored follower volumes do not replicate inboxes. Start with one
active instance and preserve its `/data` volume.

On a host whose runtime refuses every hardened exec with `operation not permitted`, investigate
the host's kernel/runtime first. In a reviewed site-specific override, removing only
`no-new-privileges` is the documented compatibility fallback; test it and record the exception.
Do not silently remove all hardening flags. Parsing the proxy config or starting a container
with weaker flags does not verify the hardened deployment.

## Monitoring and daily acceptance

`schlange-monitor.timer` checks every minute:

- `/healthz`: worker heartbeat, watchdog and journal health;
- `/readyz`: webhook admission;
- authenticated `/status`: every expected worker exists, role is leader/shadow, no halted
  train, rejected/unsupported configuration or `check_identity_unavailable` state.

The monitor reads the admin token from its file and sends it only as a header. It refuses
redirects and environment proxies and does not log response bodies. Connect systemd service
failures to the host's existing alert receiver, for example with an `OnFailure=` drop-in. A
failed timer service without a configured alert receiver is only a local log entry. Also alert
on disk pressure, backup age, timer failures and rejected GitHub webhook deliveries.

At 06:00 UTC each day, `schlange-shadow-report.timer` saves JSON reports under the private
`/var/lib/schlange/reports/` directory and fails on a disagreement or CLI error. It uses the
database locally, so no admin token is copied into subprocess arguments. Reports contain PR
metadata: protect and retain them under the project's retention policy. Any disagreement must
be reproduced, fixed and deployed before restarting the 14 consecutive day acceptance window.
The report service does nothing when `SCHLANGE_SHADOW=false`.

## Online backup and restore drill

The daily 02:15 UTC backup timer calls Python's SQLite online-backup API. It opens the live
database in read-only mode, includes committed WAL pages, verifies `integrity_check`, fsyncs
and atomically publishes a standalone `0600` backup in `/var/backups/schlange` (`0700`). It does
not stop `serve`. Do not copy only `schlange.db` while the service is running: committed data
may be in `schlange.db-wal`. Never use SQLite's `immutable=1` for this live backup.

Monitor successful backup age and free disk space. Copy backups to a separately secured,
encrypted destination on approved infrastructure; apply the project's retention policy. The
template intentionally never deletes older backups automatically. A backup on the same VPS
does not protect against VPS loss.

Periodically drill on an isolated host using read-only/shadow credentials. For a real recovery:

1. Pause the existing writer and stop `schlange.service`; verify no other process uses its
   database. Preserve the old data directory as incident evidence.
2. Verify the selected backup with Python `sqlite3` / `PRAGMA integrity_check` and record its
   timestamp. Restore the standalone backup as `/var/lib/schlange/data/schlange.db` into a new
   empty `0700` directory owned by `65532:65532`; database mode `0600`, same owner. Never pair
   it with stale `-wal` or `-shm` files.
3. Start only the restored instance. Require doctor, heartbeat and readiness to be green;
   inspect queue status, recovery log and audit against current GitHub state. Restoration
   replays uncertain intents and reconciles with GitHub; do not assume a backup rewinds GitHub.
4. Confirm pending accepted inbox deliveries process with their recorded editor identities,
   then take a fresh backup. Record the observed recovery point and duration.

For an I0 halt, stop automatic landing, preserve database/audit evidence and investigate the
tested/landed tree mismatch. Do not clear a halt or rebuild the database merely to get a green
monitor. For invalid configuration, the last valid version remains effective: correct the
default-branch configuration and verify reload. For lost leadership, find the actual lease
holder before restarting or redirecting ingress.

## Local template checks

```bash
python3 -m unittest discover -s ops/tests -v
docker compose --env-file ops/stack.env.example -f ops/compose.yaml config --quiet
systemd-analyze verify ops/systemd/schlange*.service ops/systemd/schlange*.timer
```

These check scripts and configuration. Real TLS issuance, hardened execution, egress rejection,
App permissions, alert delivery and a full restore remain required on the approved target VPS.
