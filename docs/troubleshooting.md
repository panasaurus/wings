# Troubleshooting — Panasaurus Wings v0.2.1

Common issues and their solutions. If you can't find your issue here,
check the [logs](#viewing-logs) and join the
[Discord](https://discord.gg/tszvW9zKN7) for help.

---

## Viewing Logs

### systemd

```bash
sudo journalctl -u panasaurus-wings -f
```

### Log Files (v0.2.1)

Wings writes structured logs to `<private_root>/logs/`. You can list and
read them via the API:

```bash
TOKEN=$(sudo grep '^token:' /etc/panasaurus/config.yml | awk '{print $2}')

# List log files
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8081/api/system/logs | jq .

# Read a log file
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8081/api/system/logs/wings.log | jq -r .contents
```

---

## Common Issues

### Wings won't start

#### "Docker is unavailable"

Wings can't reach the Docker daemon. Check:

```bash
sudo systemctl status docker
docker info
```

Ensure the user running Wings is in the `docker` group (or Wings is
running as root). If Docker is running but Wings still can't connect,
check the `docker.executable` config — it might point to a non-standard
path.

#### "Cannot authenticate to the panel or load node servers"

The `remote`, `token_id`, or `token` in the config is wrong. Re-run
`--configure`:

```bash
sudo PANASAURUS_TOKEN='ptla_xxx' panasaurus-wings --configure \
  --panel-url https://panel.example.com \
  --node-id 1 \
  --token-env PANASAURUS_TOKEN \
  --node-url https://node1.example.com \
  --config /etc/panasaurus/config.yml
```

If the config already exists, remove it first:

```bash
sudo rm /etc/panasaurus/config.yml
```

#### "Set real node credentials before starting"

The `token` field in the config is less than 32 characters. Generate a
real token in the panel under **Admin → Nodes → [Node] → Configuration**.

#### "The plaintext API must bind to a loopback IP"

You set `api.host` to a non-loopback IP without enabling TLS. Either:

1. Set `api.host: 127.0.0.1` and use a reverse proxy for remote access, or
2. Enable TLS:
   ```yaml
   api:
     host: 0.0.0.0
     ssl:
       enabled: true
       cert: /path/to/cert.pem
       key: /path/to/key.pem
   ```

#### "Panel credentials require HTTPS or loopback"

The `remote` URL uses HTTP on a non-loopback host. In production, always
use HTTPS. For local development, use `http://localhost:8080`.

---

### SFTP / SSH Issues

#### "Connection refused" on port 2022

SFTP is either disabled or bound to a different address/port. Check:

```yaml
system:
  sftp:
    enabled: true
    bind_address: 0.0.0.0  # or your node's public IP
    bind_port: 2022
```

If `bind_address` is `127.0.0.1`, you can only connect from the node
itself. Change it to `0.0.0.0` (or a specific interface IP) for remote
access.

#### "Permission denied" on SFTP login

The panel rejected the user's credentials, or the user doesn't have the
`file.sftp` permission for any server on this node. Check in the panel:

1. The user exists and has a valid password.
2. The user owns or has access to at least one server on this node.
3. The user's subuser permissions include `file.sftp` (or `*`).

#### `.wings` commands not working (v0.2.1)

1. Ensure the shell is enabled:
   ```yaml
   system:
     sftp:
       shell:
         enabled: true
   ```

2. Check the command prefix. The default is `.wings`, but if you changed
   `cli_name`, use that:
   ```bash
   ssh -p 2022 alice@node1 '.wings help'    # default
   ssh -p 2022 alice@node1 '.pw help'       # if cli_name is ".pw"
   ```

3. The command must start with the prefix. `ssh ... 'help'` won't work —
   use `ssh ... '.wings help'`.

4. Only `.wings` commands are allowed. Arbitrary shell commands are
   rejected by design.

#### "Server is not assigned to this node"

The server UUID the user authenticated against isn't on this node. This
happens if the panel moved the server to a different node after the SFTP
session started. Reconnect.

---

### Server Issues

#### Server won't start

1. Check the server state:
   ```bash
   ssh -p 2022 alice@node1 '.wings status'
   ```

2. Check the logs:
   ```bash
   ssh -p 2022 alice@node1 '.wings logs --limit 50'
   ```

3. Check if Docker is running the container:
   ```bash
   docker ps --filter "label=dev.panasaurus.managed=true"
   ```

4. If the container exists but isn't starting, check its logs:
   ```bash
   docker logs <container-name>
   ```

5. Common causes:
   - The server's Docker image isn't pulled. Wings pulls it on start, but
     if the registry is unreachable, it fails.
   - The startup command is wrong. Check the egg's `startup` configuration
     in the panel.
   - Port conflicts. Ensure the server's allocated ports aren't in use.

#### Server keeps crashing

Check the recovery config:

```yaml
recovery:
  enabled: true
  restart_crashed: true
  crash_cooldown_seconds: 60  # waits 60s between restart attempts
```

If `crash_cooldown_seconds` is too short, Wings may restart the server
too aggressively. The logs will show the crash reason.

#### "A server operation is already running"

Another operation (install, backup, transfer, power action) is in
progress. Wait for it to finish, or check the logs to see what's stuck.
If it's genuinely stuck, restart Wings.

---

### File Issues

#### "Disk quota exceeded"

The server has hit its disk limit. Either:

1. Increase the limit in the panel (Admin → Servers → [Server] → Build).
2. Delete files to free space.
3. If `system.default_disk_limit_mb` is too low, raise it in the config.

Check current usage:

```bash
ssh -p 2022 alice@node1 '.wings stats'
```

#### File search returns no results (v0.2.1)

1. Ensure the pattern is a valid regex. `.` matches any character; to
   match a literal dot, use `\.`.
2. Content search skips files larger than 64 MiB and binary files (non-UTF8).
3. The search starts from the `directory` parameter (default `/` = server
   root). Make sure you're searching the right directory.

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pattern":"\\.log$","mode":"filename"}' \
  http://127.0.0.1:8081/api/servers/<id>/files/search
```

#### File revision history is empty (v0.2.1)

File history is disabled by default. Enable it:

```yaml
system:
  file_history:
    enabled: true
```

Then restart Wings. Revisions are only captured for writes that happen
after enabling — existing files won't have history retroactively.

---

### Upgrade Issues (v0.2.1)

#### `POST /api/system/upgrade` returns 400

1. The URL must be HTTPS. HTTP is rejected.
2. If a checksum is provided, it must be exactly 64 hex characters
   (sha256).

#### Upgrade succeeds but version doesn't change

The upgrade downloads and swaps the binary, but **a service restart is
required to activate it**. Restart Wings:

```bash
sudo systemctl restart panasaurus-wings
```

#### Checksum mismatch

The downloaded binary's sha256 doesn't match the provided checksum. Either
the checksum is wrong, or the download was corrupted/truncated. Re-download
and recompute:

```bash
sha256sum /usr/local/bin/panasaurus-wings
```

---

### Performance Issues

#### High CPU usage

1. Check per-server stats:
   ```bash
   ssh -p 2022 alice@node1 '.wings stats'
   ```

2. Check node-wide stats (v0.2.1):
   ```bash
   curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8081/api/stats | jq .
   ```

3. File search and file history can be CPU-intensive. Disable them if not
   needed:
   ```yaml
   system:
     file_history:
       enabled: false
   ```

#### High disk usage

1. Check file history disk budget:
   ```yaml
   system:
     file_history:
       per_server_disk_budget: 268435456  # 256 MiB per server
   ```

2. Old log files accumulate in `<private_root>/logs/`. Clean them up
   periodically or set up log rotation.

3. Docker images and containers can accumulate. Prune unused ones:
   ```bash
   docker system prune -f
   ```

---

### Getting Help

If none of the above helps:

1. Collect diagnostics:
   ```bash
   panasaurus-wings --diagnostics --config /etc/panasaurus/config.yml
   ```

2. Gather the logs:
   ```bash
   sudo journalctl -u panasaurus-wings --since "1 hour ago" > wings-logs.txt
   ```

3. Join the [Discord](https://discord.gg/tszvW9zKN7) and ask in the
   `#support` channel with the diagnostics output and logs attached.

4. File a bug at [github.com/panasaurus/panel/issues](https://github.com/panasaurus/panel/issues).
