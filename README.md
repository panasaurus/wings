# Panasaurus Wings

Rust daemon for the Panasaurus panel. Docker containers use Panasaurus images supplied by their eggs. Existing opt-in Minecraft hibernation remains available.

## Run

Install a current Rust toolchain and Docker, copy `config.example.yml`, and insert the node credentials from the panel. Set an absolute `system.data` directory accessible to Docker. Linux storage requires root, `e2fsprogs`, `mount`, `findmnt`, and `losetup`; macOS uses `hdiutil`.

```sh
cargo build --locked --release
./target/release/panasaurus-wings --config /absolute/path/config.yml
./target/release/panasaurus-wings --diagnostics --config /absolute/path/config.yml
```

Plain HTTP binds to loopback. Use a TLS reverse proxy for remote API access, or enable native TLS with absolute certificate/key paths. `jwt_audience` must match the panel's node connection address, including its port. Certificates can also be provisioned with `api.ssl.acme_domain`, `acme_email`, and `acme_production: true`; the domain must resolve to the node and public port 443 must deliver TLS-ALPN challenges to this listener. ACME starts with the staging directory unless production is explicitly enabled.

SFTP defaults to `127.0.0.1:2022`. Set `system.sftp.bind_address` to the intended interface for remote SFTP. It is a separate SSH service; an HTTP tunnel does not forward SFTP. Use the panel's `username.server-uuid` login, panel password, or an uploaded Ed25519/ECDSA public key. The daemon persists a private Ed25519 host key. RSA and legacy DSA SSH algorithms are disabled. SSH shells and port forwarding are unavailable.

## Features

- All 38 routes in the Pterodactyl Wings v1.13.3 HTTP contract, plus existing Panasaurus create/update file routes.
- Egg shutdown commands/signals, graceful restarts, readiness matching, live console and installation events, bounded installation jobs and timeouts.
- Properties, JSON, YAML, INI, XML, and text egg configuration parsers, conditional replacements, arrays, wildcards and supported public node placeholders.
- Crash/OOM recovery with a cooldown and persistent desired power state. An explicitly stopped server stays off. Hibernating servers retain their wake policy.
- Local and panel-coordinated S3 multipart backups, checksum verification, signed downloads, restoration, deletion and retained rollback archives if a restore fails.
- Large streaming uploads/downloads with scoped, expiring, single-use JWTs; mkdir, rename, copy, chmod, tar.gz creation, tar/tar.gz/ZIP extraction; remote downloads with progress and cancellation.
- Panel-authenticated SFTP with file permission checks and session revocation.
- Source/destination node transfers, SHA-256 integrity checks, staging, cancellation and panel completion callbacks. Destination data is retained if a completion callback has an uncertain result.
- Offline disk quota resizing and deletion of the owned quota image and server tree.
- Approved host mounts, dedicated Docker bridge networks, DNS, PID/tmpfs/CPU/memory tuning and registry credentials supplied in the operator configuration.
- Atomic private configuration persistence, dynamic CORS, native TLS/ACME, configuration and diagnostic commands, and systemd unit output.

## Safety and configuration

Filesystem operations walk capabilities and refuse traversal, special files, symlink aliases, hard-linked files, protected egg paths, and special permission bits. Archives are staged and validated before writing, with depth, entry-count and expanded-size bounds. Browser uploads, archive extraction and remote pulls cannot overwrite existing files without the required update authorization; extraction uses create-only semantics. Backups/transfers may include protected regular game files through their separate administrative operation.

Remote destinations are resolved and pinned. Private, loopback, link-local, reserved and cloud metadata addresses are blocked, including redirect destinations. An exact host in `downloads.allowed_hosts` is an explicit operator exception, useful for private S3/MinIO endpoints and local node transfers. Avoid adding tenant-controlled hosts to this exception list. Redirects cannot downgrade HTTPS.

`allowed_mounts` is an operator whitelist. Each mount source must resolve within it and outside node/server storage. Mount targets cannot cover protected system directories. Node update requests preserve local storage/executable paths and this whitelist. Listener and concurrency changes return `x-panasaurus-restart-required: true`; restart the daemon after these updates. Credentials, remote URL, permitted origins and download policy update immediately.

Backups and temporary work live in the private `.panasaurus-node` directory outside each container's mounted payload. Failed restores retain a `restore-*/rollback.tar.gz` recovery archive there for an operator to inspect. Archive operations intentionally reject aliases and special files rather than following them.

Quota changes require an offline server; running container changes return a conflict until a restart is requested. Filesystem tools reject an infeasible shrink. Migration backups remain until the corresponding server is deleted. Keep ordinary host backups of game data.

The `cpu_burst_percent` setting is reserved and must remain zero. CPU quotas, scheduler period, shares and memory overhead are supported; realtime scheduling is never enabled.

## Configure and install a service

Provide an application API token through an environment variable, not a command argument. The destination config must not already exist.

```sh
./target/release/panasaurus-wings --configure \
  --panel-url https://panel.example.com --node-id 1 \
  --token-env PANASAURUS_APPLICATION_TOKEN \
  --node-url https://node.example.com:8081 \
  --config /etc/panasaurus/config.yml
./target/release/panasaurus-wings --systemd-unit --config /etc/panasaurus/config.yml
```

The configure command starts with a loopback HTTP listener suitable for a TLS reverse proxy. Review Docker identity, storage paths and listener settings before starting it. The systemd command prints a unit for installation by the host operator; it does not change system services automatically.

## Release validation

Production builds are checked with formatting, Clippy, unit tests, isolated integration tests and a dependency advisory audit before publishing. Test fixtures and local runtime data are excluded from releases. Linux requires mounting privileges and the filesystem tools listed above. ACME issuance and production S3 credentials require deployment-specific validation. A clean dependency audit reports known advisories; it does not prove that software has no vulnerabilities.
