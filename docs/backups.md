# Backups Guide — Panasaurus Wings v0.2.1

Wings supports creating, restoring, and deleting server backups. Backups
are triggered by the panel via the HTTP API and run as background jobs
subject to the `limits.jobs` concurrency cap.

---

## How Backups Work

When the panel calls `POST /api/servers/{id}/backup`, Wings:

1. Acquires a job slot (from the `limits.jobs` semaphore).
2. Locks the server's operation mutex (prevents concurrent power actions).
3. Creates a tar.gz archive of the server's `/home/container/` directory,
   excluding the egg's `ignored_files` patterns.
4. Streams the archive to the panel's backup upload endpoint.
5. Notifies the panel of success/failure via the installation callback.

The server continues running during the backup — files are read
consistently (no snapshot freeze), so in-flight writes may or may not be
captured depending on timing.

---

## API Endpoints

### Create a Backup

```
POST /api/servers/{id}/backup
Authorization: Bearer <node-token>
```

Returns `202 Accepted` immediately; the backup runs in the background.

### Delete a Backup

```
DELETE /api/servers/{id}/backup/{backup-uuid}
Authorization: Bearer <node-token>
```

### Restore a Backup

```
POST /api/servers/{id}/backup/{backup-uuid}/restore
Authorization: Bearer <node-token>
```

Stops the server, replaces `/home/container/` with the backup contents,
and (optionally) restarts the server.

### Download a Backup

```
GET /download/backup?token=<jwt>&backup=<backup-uuid>
```

The JWT is issued by the panel and is short-lived. The endpoint streams
the backup archive directly to the client.

---

## Backup Format

Wings v0.2.1 uses the local `wings` backup driver, which creates
`tar.gz` archives. The archive contains the full `/home/container/`
directory tree, preserving permissions and symlinks (within the sandbox's
safety rules — no symlink escapes).

Other backup drivers from wings-main (S3, restic, btrfs, zfs, pbs,
ddup-bak) are not included in v0.2.1 to keep the dependency footprint
small. They may be added in future versions.

---

## Configuration

Backups use the following config sections:

```yaml
limits:
  jobs: 8  # Max concurrent backup/restore/install jobs

system:
  data: /var/lib/panasaurus/volumes  # Server data lives here

docker:
  installer_limits:
    memory: 1024   # MiB — also used for backup restore containers
    cpu: 100       # percent
```

There is no separate `backups` config section in v0.2.1 — backup behavior
is controlled by the panel (which specifies the backup UUID, ignored
files, and upload endpoint).

---

## File Revision History (NEW in v0.2.1)

In addition to full backups, Wings v0.2.1 can track per-file revision
history. This is a lighter-weight alternative for "undo" scenarios —
users can browse and restore individual file revisions without restoring
a full backup.

### Enabling File History

```yaml
system:
  file_history:
    enabled: true
    zstd_level: 19
    file_size_cap: 1048576       # 1 MiB — files larger than this aren't tracked
    keep_chains: 10              # max revisions per file
    per_server_disk_budget: 268435456  # 256 MiB per server
```

### File History API

```
GET /api/servers/{id}/files/revisions?file=<path>
```

Returns a list of revisions (newest first):

```json
{
  "file": "server.properties",
  "revisions": [
    { "id": "1698123456789-a1b2c3d4e5f6a7b8", "timestamp": 1698123456789, "size": 1234 },
    { "id": "1698120000000-b2c3d4e5f6a7b8c9", "timestamp": 1698120000000, "size": 1200 }
  ]
}
```

```
GET /api/servers/{id}/files/revisions/{revision}?file=<path>
```

Returns the revision contents (base64-encoded):

```json
{
  "file": "server.properties",
  "revision": "1698123456789-a1b2c3d4e5f6a7b8",
  "size": 1234,
  "contents_base64": "..."
}
```

### Storage

Revisions are stored at:
```
<system.data>/.panasaurus-node/file_history/<server_uuid>/<file_hash>/<timestamp>-<digest>.zst
```

Each revision is zstd-compressed at the configured level. The oldest
revisions are pruned when `keep_chains` is exceeded, and a per-server disk
budget prevents runaway growth.

---

## Best Practices

1. **Schedule backups during low-traffic hours.** Backups read the full
   server directory, which can cause I/O contention on busy servers.

2. **Use file history for frequent, lightweight snapshots.** It's cheaper
   than a full backup and lets users self-serve file restores.

3. **Monitor disk space.** Backups are streamed to the panel, but file
   history lives on the node. Set `per_server_disk_budget` to prevent
   runaway growth.

4. **Test restores.** A backup you can't restore is not a backup.
   Periodically restore a backup to a test server to verify integrity.

5. **Keep `limits.jobs` reasonable.** Setting it too high can starve the
   node of I/O during concurrent backups. The default (8) is a good
   starting point for most nodes.
