# SFTP + SSH CLI Guide — Panasaurus Wings v0.2.1

Wings exposes two services over SSH on the same port (default 2022):

1. **SFTP** — file transfer, used by FileZilla, WinSCP, Cyberduck, etc.
2. **SSH `.wings` CLI** — server management commands (start, stop, status,
   stats, logs), new in v0.2.1.

Both authenticate against the panel — users connect with their panel
username and password (or public key, if the panel supports it).

---

## SFTP — File Transfer

### Connecting

```
Host: <node-fqdn-or-ip>
Port: 2022
Protocol: SFTP
Username: <panel-username>
Password: <panel-password>
```

The user is chrooted to their server's `/home/container/` directory.
Symlinks are forbidden (they'd escape the chroot), and the egg's
`file_denylist` is enforced.

### Supported SFTP Operations

All standard SFTP v3 operations are supported: `open`, `opendir`, `read`,
`write`, `close`, `mkdir`, `rmdir`, `remove`, `rename`, `readlink`,
`symlink`, `setstat`, `fsetstat`, `fchmod`, `fchown`, `truncate`,
`ftruncate`, `stat`, `lstat`, `fstat`, `realpath`.

### SFTP Extensions (NEW in v0.2.1)

Wings v0.2.1 implements the following OpenSSH SFTP extensions:

| Extension | Description |
|-----------|-------------|
| `statvfs@openssh.com` | Filesystem statistics (total/free/available blocks + inodes). |
| `space-available` | Alias for `statvfs@openssh.com`. |
| `hardlink@openssh.com` | Create a hard link (same as `ln` without `-s`). |
| `posix-rename@openssh.com` | Atomic rename that overwrites the destination (unlike base SFTP `rename`). |
| `copy-file` | Server-side file copy (no data round-trips over the wire). |
| `fsync@openssh.com` | Flush a file handle's data to disk. |

Clients that support these extensions (OpenSSH `sftp`, WinSCP, Cyberduck)
will use them automatically. Unsupported extensions return
`SSH_FX_OP_UNSUPPORTED`.

### Permissions

The panel grants SFTP permissions per user per server. The relevant
permissions:

- `file.sftp` — required to connect via SFTP at all
- `file.read` — read files
- `file.read-content` — read file contents
- `file.create` — create files/directories
- `file.update` — modify files, rename, chmod
- `file.delete` — delete files/directories
- `file.archive` — compress/decompress archives
- `*` — wildcard (all permissions)

Mutations (write, delete, rename, etc.) are blocked while a server is
installing or transferring.

### Limits

- Max concurrent SFTP connections: `limits.sftp_connections` (default 64)
- Max open handles per session: `system.sftp.max_handles` (default 64)
- Max file path depth: 128 components
- Max file path length: 4096 bytes
- Max traversal entries: 100,000

---

## SSH `.wings` CLI (NEW in v0.2.1)

The `.wings` CLI lets users manage their server from a terminal without
opening the panel. Connect with:

```bash
ssh -p 2022 <panel-username>@<node-host> '.wings help'
```

Enter your panel password when prompted. The CLI runs the command, prints
output, and disconnects.

### Enabling / Disabling

The CLI is enabled by default. To disable it:

```yaml
system:
  sftp:
    shell:
      enabled: false
```

You can also change the command prefix (default `.wings`):

```yaml
system:
  sftp:
    shell:
      cli_name: ".pw"  # users would type: ssh ... '.pw help'
```

### Command Reference

#### `.wings help`

Show available commands.

```
$ ssh -p 2022 alice@node1 '.wings help'
Available commands:
  help         - Show this help message
  version      - Show the running wings version
  status       - Show the server state
  power        - Send a power action to the server
  stats        - Show current resource utilization for the server
  logs         - Show recent console output for the server
  permissions  - Show the permissions granted to your SFTP session
```

#### `.wings version`

Show the wings version, OS, and architecture.

```
$ ssh -p 2022 alice@node1 '.wings version'
Panasaurus Wings v0.2.1 (linux/x86_64)
```

#### `.wings status [--json]`

Show the server's current state (`offline`, `starting`, `running`,
`stopping`).

```
$ ssh -p 2022 alice@node1 '.wings status'
State: running
```

With `--json`:

```
$ ssh -p 2022 alice@node1 '.wings status --json'
{"state":"running","server":"abc12345-..."}
```

#### `.wings power <start|restart|stop|kill>`

Send a power action. Requires the matching `power.<action>` permission.

```
$ ssh -p 2022 alice@node1 '.wings power start'
Power action `start` accepted.
```

```
$ ssh -p 2022 alice@node1 '.wings power stop'
Power action `stop` accepted.
```

If you lack permission:

```
$ ssh -p 2022 alice@node1 '.wings power kill'
You are missing the `power.kill` permission to do this.
```

#### `.wings stats [--json]`

Show current resource utilization (CPU, memory, disk, uptime). Requires
`control.stats` or `control` permission.

```
$ ssh -p 2022 alice@node1 '.wings stats'
State:   running
Uptime:  2h 34m 12s
CPU:     15.3%
Memory:  1.2 GB / 4.0 GB
Disk:    8.5 GB
```

With `--json`, returns the raw utilization object from the Docker stats
API (plus `state` and `hibernation` fields).

#### `.wings logs [--limit N]`

Show recent console output. Default 50 lines; max 1000. Requires
`control.console` or `control` permission.

```
$ ssh -p 2022 alice@node1 '.wings logs --limit 5'
[12:00:01] Starting minecraft server...
[12:00:03] Loading libraries
[12:00:05] Starting Minecraft server on *:25565
[12:00:06] Done (3.452s)! For help, type "help"
```

#### `.wings permissions`

Show the permissions the panel granted to your current SFTP session.

```
$ ssh -p 2022 alice@node1 '.wings permissions'
Granted permissions:
  - file.sftp
  - file.read
  - file.read-content
  - file.create
  - file.update
  - power.start
  - power.stop
  - control.console
  - control.stats
```

### Security Notes

- The `.wings` CLI only accepts commands starting with the configured
  prefix (`.wings` by default). Arbitrary shell commands are rejected.
- Each command respects the user's panel-granted permissions. A user
  without `power.start` cannot start the server.
- Power actions are audit-logged to the panel's activity feed.
- The CLI runs inline (no background tasks), so it's safe to call
  repeatedly.

### Using SSH Keys

If the panel supports public-key authentication for SFTP, the same keys
work for the `.wings` CLI:

```bash
ssh -p 2022 -i ~/.ssh/id_ed25519 alice@node1 '.wings status'
```

### Exit Status

The CLI sets the SSH exit status to `0` on success and `1` on failure
(permission denied, server not found, etc.). This lets you script around
it:

```bash
if ssh -p 2022 alice@node1 '.wings power start'; then
  echo "Server started successfully"
else
  echo "Failed to start server"
fi
```
