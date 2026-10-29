# Build Notes — Panasaurus Wings v0.2.1

This document captures known considerations for building v0.2.1 from
source. The release was developed by porting features from the Calagopus
Wings project into the Panasaurus Wings codebase. While the code follows
existing patterns from v0.2.0 and the wings-main reference, it was
written without a local Rust toolchain to verify compilation end-to-end.

If you hit a compile error, this document explains the most likely causes
and how to fix them.

---

## Requirements

- **Rust 1.85+** (edition 2024)
- **C compiler** + **cmake** (for `aws-lc-rs`, used by `rustls`/`jsonwebtoken`)
- **pkg-config** + **OpenSSL dev headers** (alternative TLS backend path)

On Debian/Ubuntu:
```bash
sudo apt install build-essential cmake pkg-config libssl-dev clang
```

---

## Building

```bash
cargo build --release
```

A clean release build takes ~5–10 minutes on a modern machine due to
`aws-lc-rs` and the LTO pass.

---

## Potential Compile Issues

The following are the most likely places a first compile might fail, with
fixes. If you hit something not listed here, check the
[troubleshooting guide](docs/troubleshooting.md) or file an issue.

### 1. `russh_sftp::protocol::Packet` variant names

The SFTP extensions handler in [`src/sftp.rs`](src/sftp.rs) constructs
`Packet::ExtendedReply(ExtendedReply { id, data })` and
`Packet::Status(Status { ... })`. If the `russh-sftp` 3.x crate uses
different variant names (e.g. `Packet::ExtendedReply` vs
`Packet::Extended_reply`), adjust the match arms in the `extended`
method.

**Fix**: Check `russh_sftp::protocol::Packet` enum variants in the
crate docs and update the variant names in `src/sftp.rs`:

```rust
// If the variant is named differently:
Ok(russh_sftp::protocol::Packet::Status(ok(id)))
// vs
Ok(russh_sftp::protocol::Packet::Status(Status { ... }))
```

### 2. `russh_sftp::de::from_bytes` visibility

The SFTP extensions use `russh_sftp::de::from_bytes` to parse wire data,
matching the wings-main reference. If this module is private in the
published `russh-sftp` 3.x crate, replace with manual parsing:

```rust
// Manual parser for two SSH strings (used by hardlink/posix-rename/copy-file):
fn parse_two_strings(data: &[u8]) -> Result<(String, String), StatusCode> {
    if data.len() < 4 { return Err(StatusCode::BadMessage); }
    let len1 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len1 + 4 { return Err(StatusCode::BadMessage); }
    let s1 = std::str::from_utf8(&data[4..4+len1])
        .map_err(|_| StatusCode::BadMessage)?.to_owned();
    let rest = &data[4+len1..];
    let len2 = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    if rest.len() < 4 + len2 { return Err(StatusCode::BadMessage); }
    let s2 = std::str::from_utf8(&rest[4..4+len2])
        .map_err(|_| StatusCode::BadMessage)?.to_owned();
    Ok((s1, s2))
}
```

### 3. `russh::server::Session` method names

The SSH CLI handler in [`src/sftp.rs`](src/sftp.rs) calls
`session.exit_status_request(id, exit_status)`. If the russh 0.64 API
uses a different method name (e.g. `exit_status` or
`set_exit_status`), update the call. The `let _ = ...` pattern means a
wrong method name will produce a compile error (not a runtime error),
which is easy to spot and fix.

**Fix**: Check `russh::server::Session` methods in the crate docs. The
method for sending an exit status to the client may be named
`exit_status_request`, `exit_status`, or `set_exit_status`.

### 4. `cap_std::fs::Dir` method names

The SFTP extensions in [`src/sftp_extensions.rs`](src/sftp_extensions.rs)
use `dir.hard_link(...)`, `dir.rename(...)`, `dir.open(...)`, and
`dir.create(...)`. These are standard `cap_std::fs::Dir` methods, but if
the cap_std 4.x API differs, consult the crate docs.

**Fix**: `cap_std::fs::Dir` methods are stable across 3.x/4.x. If
`create` doesn't exist, use `OpenOptions::new().write(true).create_new(true)`
via `dir.open_with(...)`.

### 5. `ExtendedReply.data` field type

The `data` field in `russh_sftp::protocol::ExtendedReply` may be
`Vec<u8>`, `bytes::Bytes`, or a similar type. The code uses `.into()` to
convert from `Vec<u8>`, which should work for most types. If not:

**Fix**: Change `data: stats.to_wire().into()` to explicitly construct
the expected type:
```rust
data: stats.to_wire(),  // if data is Vec<u8>
// or
data: bytes::Bytes::from(stats.to_wire()),  // if data is bytes::Bytes
```

### 6. Missing `use` imports

Some new modules use types that may need additional `use` statements
depending on how the crate re-exports them. If you see "cannot find type
X in this scope", add the appropriate `use` at the top of the file.

### 7. `grep-regex` / `grep-regex` crate API

The file search module ([`src/file_search.rs`](src/file_search.rs))
currently uses the `regex` crate directly (not `grep-regex`). If you want
to switch to `grep-regex` for better performance on large files, the API
is different (it uses a `Searcher` + `Matcher` pattern). The current
`regex`-based implementation is simpler and sufficient for most use
cases.

---

## Dependency Notes

### `utoipa`

Added for OpenAPI support, but the v0.2.1 implementation uses a
hand-crafted JSON spec instead of utoipa derive macros (to avoid
annotating every handler). The `utoipa` crate is included as a
dependency for future use but is not currently exercised. If it causes
build issues, it can be removed from `Cargo.toml` without affecting
functionality — the `/openapi.json` endpoint is served by
[`src/openapi.rs`](src/openapi.rs) which only depends on `serde_json`.

### `walkdir`

Used by the file_history budget enforcement to walk the history
directory. If `walkdir` causes issues, it can be replaced with a manual
recursive `std::fs::read_dir` walk.

### `rayon`

Used by the file search for parallel content scanning. The rayon thread
pool is global (default size = num CPUs). If this causes issues with the
tokio runtime, the search can be made sequential by replacing
`files.par_iter()` with `files.iter()`.

### `zstd`

Used by file_history for compression. The `zstd` crate bundles the C
library, so no system dependency is needed. If the build fails on the C
compilation, ensure `cmake` and a C compiler are installed.

---

## Verifying the Build

After a successful build:

```bash
# Check the version
./target/release/panasaurus-wings --version
# Should print: Panasaurus Wings Rust 0.2.1

# Check help output
./target/release/panasaurus-wings --help

# Run diagnostics (requires a valid config)
./target/release/panasaurus-wings --diagnostics --config /etc/panasaurus/config.yml
```

---

## Reporting Build Issues

If you hit a compile error not covered here, please file an issue at
[github.com/panasaurus/panel/issues](https://github.com/panasaurus/panel/issues)
with:

1. The exact error message (full compiler output).
2. Your Rust version (`rustc --version`).
3. Your OS and architecture.
4. The output of `cargo tree | head -50`.
