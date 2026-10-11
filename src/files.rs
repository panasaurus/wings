use crate::{
    State,
    api::{ApiError, ApiResult},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State as AppState},
    http::StatusCode,
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, MetadataExt, OpenOptions, OpenOptionsExt},
};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

const EDIT_LIMIT: u64 = 1024 * 1024;

#[derive(Deserialize)]
pub struct FileQuery {
    file: String,
}

#[derive(Clone, Copy)]
enum Mode {
    Write,
    Create,
    Update,
}

fn path(file: &str) -> io::Result<PathBuf> {
    relative_path(file, false)
}

fn relative_path(file: &str, allow_root: bool) -> io::Result<PathBuf> {
    let input = FsPath::new(file.trim_start_matches('/'));
    let mut clean = PathBuf::new();
    for component in input.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Invalid server file path",
                ));
            }
        }
    }
    if clean.as_os_str().is_empty() && !allow_root {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Invalid server file path",
        ));
    }
    Ok(clean)
}

fn parent(directory: &FsPath, file: &FsPath) -> io::Result<(Dir, PathBuf)> {
    let mut dir = Dir::open_ambient_dir(directory, ambient_authority())?;
    let parts: Vec<_> = file.components().collect();
    for component in &parts[..parts.len() - 1] {
        dir = dir.open_dir_nofollow(component.as_os_str())?;
    }
    Ok((dir, PathBuf::from(parts.last().unwrap().as_os_str())))
}

fn read_file(directory: &FsPath, file: &str) -> io::Result<Vec<u8>> {
    let dir = Dir::open_ambient_dir(directory, ambient_authority())?;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    let input = dir.open_with(path(file)?, &options)?;
    if !input.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Only regular files can be edited",
        ));
    }
    let mut content = Vec::new();
    input.take(EDIT_LIMIT + 1).read_to_end(&mut content)?;
    if content.len() as u64 > EDIT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "File exceeds the edit limit",
        ));
    }
    Ok(content)
}

pub(crate) fn disk_usage(dir: &Dir) -> io::Result<u64> {
    scan_usage(dir, 0, &mut 100_000)
}

fn scan_usage(dir: &Dir, depth: usize, remaining: &mut usize) -> io::Result<u64> {
    if depth > 128 {
        return Err(io::Error::other("Server file tree is too deep"));
    }
    let mut total: u64 = 0;
    for entry in dir.entries()? {
        if *remaining == 0 {
            return Err(io::Error::other("Server file tree exceeds the scan limit"));
        }
        *remaining -= 1;
        let entry = entry?;
        let metadata = entry.metadata()?;
        let size = if metadata.is_dir() {
            scan_usage(
                &dir.open_dir_nofollow(entry.file_name())?,
                depth + 1,
                remaining,
            )?
        } else if metadata.is_file() {
            metadata.len()
        } else {
            0
        };
        total = total
            .checked_add(size)
            .ok_or_else(|| io::Error::other("Disk usage overflow"))?;
    }
    Ok(total)
}

fn write_file(
    directory: &FsPath,
    file: &str,
    body: &[u8],
    mode: Mode,
    denylist: &[String],
    quota: u64,
) -> io::Result<()> {
    let file = path(file)?;
    let mut builder = GitignoreBuilder::new(directory);
    for pattern in denylist {
        builder.add_line(None, pattern).map_err(io::Error::other)?;
    }
    let denylist = builder.build().map_err(io::Error::other)?;
    if denylist
        .matched_path_or_any_parents(&file, false)
        .is_ignore()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "This file is protected by the game template",
        ));
    }
    let root = Dir::open_ambient_dir(directory, ambient_authority())?;
    let (dir, name) = parent(directory, &file)?;
    let existing = match dir.symlink_metadata(&name) {
        Ok(meta) => {
            if !meta.is_file() || meta.nlink() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Only unlinked regular files can be edited",
                ));
            }
            Some(meta.len())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    match mode {
        Mode::Create if existing.is_some() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "The file already exists",
            ));
        }
        Mode::Update if existing.is_none() => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The file does not exist",
            ));
        }
        _ => {}
    }
    let used = disk_usage(&root)?;
    let projected = used
        .saturating_sub(existing.unwrap_or(0))
        .checked_add(body.len() as u64)
        .ok_or_else(|| io::Error::other("Disk usage overflow"))?;
    if projected > quota {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "Server disk quota exceeded",
        ));
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(matches!(mode, Mode::Write))
        .create_new(matches!(mode, Mode::Create))
        .custom_flags(libc::O_NONBLOCK)
        .follow(FollowSymlinks::No);
    let mut output = dir.open_with(&name, &options)?;
    let meta = output.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Only unlinked regular files can be edited",
        ));
    }
    output.set_len(0)?;
    output.write_all(body)
}

#[derive(Deserialize)]
pub struct DeleteRequest {
    #[serde(default = "root_directory")]
    root: String,
    files: Vec<String>,
}

fn walk_delete(
    dir: &Dir,
    name: &FsPath,
    relative: &FsPath,
    denylist: &Gitignore,
    apply: bool,
    depth: usize,
    remaining: &mut usize,
) -> io::Result<()> {
    let meta = match dir.symlink_metadata(name) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if depth > 128 || *remaining == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deletion exceeds the traversal limit",
        ));
    }
    *remaining -= 1;
    if denylist
        .matched_path_or_any_parents(relative, meta.is_dir())
        .is_ignore()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "This file is protected by the game template",
        ));
    }
    if meta.is_dir() {
        let child = dir.open_dir_nofollow(name)?;
        for entry in child.entries()? {
            let name = PathBuf::from(entry?.file_name());
            walk_delete(
                &child,
                &name,
                &relative.join(&name),
                denylist,
                apply,
                depth + 1,
                remaining,
            )?;
        }
        if apply {
            dir.remove_dir(name)?;
        }
    } else if apply {
        dir.remove_file(name)?;
    }
    Ok(())
}

fn delete_files(directory: &FsPath, request: DeleteRequest, denylist: &[String]) -> io::Result<()> {
    if request.files.is_empty() || request.files.len() > 10_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Select between one and 10000 files",
        ));
    }
    let root = relative_path(&request.root, true)?;
    let mut targets = request
        .files
        .iter()
        .map(|name| path(name).map(|file| root.join(file)))
        .collect::<io::Result<Vec<_>>>()?;
    targets.sort();
    let mut unique = Vec::<PathBuf>::new();
    let mut selected = std::collections::HashSet::new();
    for target in targets {
        if !target
            .ancestors()
            .any(|ancestor| selected.contains(ancestor))
        {
            selected.insert(target.clone());
            unique.push(target);
        }
    }
    let mut builder = GitignoreBuilder::new(directory);
    for pattern in denylist {
        builder.add_line(None, pattern).map_err(io::Error::other)?;
    }
    let denylist = builder.build().map_err(io::Error::other)?;
    for apply in [false, true] {
        let mut remaining = 100_000;
        for relative in &unique {
            let (dir, name) = match parent(directory, relative) {
                Ok(target) => target,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e)
                    if e.kind() == io::ErrorKind::NotADirectory
                        || e.raw_os_error() == Some(libc::ELOOP) =>
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Directory aliases cannot be traversed",
                    ));
                }
                Err(e) => return Err(e),
            };
            walk_delete(&dir, &name, relative, &denylist, apply, 0, &mut remaining)?;
        }
    }
    Ok(())
}

pub async fn delete(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(request): Json<DeleteRequest>,
) -> ApiResult<StatusCode> {
    let server = state.server(&id).await?;
    let operation = server.operation.clone().lock_owned().await;
    if server.installing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Wait for installation to finish before deleting files.".into(),
        ));
    }
    let settings = server.settings.read().await;
    let denylist: Vec<String> = settings["egg"]["file_denylist"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    drop(settings);
    let directory = state.engine.directory(&id)?;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        delete_files(&directory, request, &denylist)
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn error(error: io::Error) -> ApiError {
    let status = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        io::ErrorKind::StorageFull => StatusCode::INSUFFICIENT_STORAGE,
        io::ErrorKind::InvalidInput => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, "The server file could not be accessed.".into())
}

pub async fn contents(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> ApiResult<Bytes> {
    state.server(&id).await?;
    let directory = state.engine.directory(&id)?;
    let body = tokio::task::spawn_blocking(move || read_file(&directory, &query.file))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(error)?;
    Ok(Bytes::from(body))
}

async fn edit(
    state: Arc<State>,
    id: String,
    query: FileQuery,
    body: Bytes,
    mode: Mode,
) -> ApiResult<StatusCode> {
    let server = state.server(&id).await?;
    let operation = server.operation.clone().lock_owned().await;
    if server.installing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Wait for installation to finish before editing files.".into(),
        ));
    }
    let settings = server.settings.read().await;
    let denylist: Vec<String> = settings["egg"]["file_denylist"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let quota = crate::storage::limit(&state.engine.config, &settings)?;
    drop(settings);
    let directory = state.engine.directory(&id)?;
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        write_file(&directory, &query.file, &body, mode, &denylist, quota)
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn write(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    edit(state, id, query, body, Mode::Write).await
}
pub async fn create(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    edit(state, id, query, body, Mode::Create).await
}
pub async fn update(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    edit(state, id, query, body, Mode::Update).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn write(dir: &FsPath, file: &str, data: &[u8]) -> io::Result<()> {
        write_file(dir, file, data, Mode::Write, &[], u64::MAX)
    }
    #[test]
    fn edits_are_confined_to_server_directory() {
        let server = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("private.txt");
        std::fs::write(&secret, "untouched").unwrap();
        symlink(&secret, server.path().join("escape.txt")).unwrap();
        symlink(outside.path(), server.path().join("escape-dir")).unwrap();
        for file in [
            "../private.txt",
            "/../private.txt",
            "escape.txt",
            "escape-dir/private.txt",
            "escape-dir/new.txt",
        ] {
            assert!(write(server.path(), file, b"changed").is_err());
            assert!(read_file(server.path(), file).is_err());
        }
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "untouched");
        assert!(!outside.path().join("new.txt").exists());
        write(server.path(), "/settings.txt", b"test value").unwrap();
        write(server.path(), "settings.txt", b"new").unwrap();
        assert_eq!(read_file(server.path(), "/settings.txt").unwrap(), b"new");
        assert!(write(server.path(), "/", b"invalid").is_err());
        std::fs::write(
            server.path().join("large.txt"),
            vec![0u8; EDIT_LIMIT as usize + 1],
        )
        .unwrap();
        assert!(read_file(server.path(), "large.txt").is_err());
    }
    #[test]
    fn create_cannot_overwrite_and_update_cannot_create() {
        let server = tempfile::tempdir().unwrap();
        write_file(server.path(), "file", b"original", Mode::Create, &[], 100).unwrap();
        assert_eq!(
            write_file(server.path(), "file", b"attack", Mode::Create, &[], 100)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(read_file(server.path(), "file").unwrap(), b"original");
        assert_eq!(
            write_file(server.path(), "missing", b"attack", Mode::Update, &[], 100)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        write_file(server.path(), "file", b"update", Mode::Update, &[], 100).unwrap();
    }
    #[test]
    fn protected_files_patterns_and_aliases_are_rejected() {
        let server = tempfile::tempdir().unwrap();
        std::fs::create_dir(server.path().join("protected")).unwrap();
        std::fs::create_dir(server.path().join("allowed")).unwrap();
        std::fs::write(server.path().join("protected.txt"), "marker").unwrap();
        symlink("protected.txt", server.path().join("alias")).unwrap();
        symlink("protected", server.path().join("alias-dir")).unwrap();
        std::fs::hard_link(
            server.path().join("protected.txt"),
            server.path().join("hardlink"),
        )
        .unwrap();
        let denylist = vec![
            "protected.txt".into(),
            "protected/".into(),
            "*.secret".into(),
            "!allowed/public.secret".into(),
        ];
        for file in [
            "protected.txt",
            "./protected.txt",
            "/protected.txt",
            "protected/new",
            "x.secret",
            "alias",
            "alias-dir/new",
            "hardlink",
        ] {
            assert!(
                write_file(server.path(), file, b"attack", Mode::Write, &denylist, 1000).is_err(),
                "Allowed {file}"
            );
        }
        assert_eq!(
            read_file(server.path(), "protected.txt").unwrap(),
            b"marker"
        );
        write_file(
            server.path(),
            "allowed/public.secret",
            b"okay",
            Mode::Create,
            &denylist,
            1000,
        )
        .unwrap();
    }
    #[test]
    fn quota_counts_other_files_and_replacement_size() {
        let server = tempfile::tempdir().unwrap();
        write_file(server.path(), "a", &[0; 60], Mode::Write, &[], 100).unwrap();
        assert_eq!(
            write_file(server.path(), "b", &[0; 60], Mode::Write, &[], 100)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!server.path().join("b").exists());
        write_file(server.path(), "a", &[0; 40], Mode::Update, &[], 100).unwrap();
        write_file(server.path(), "b", &[0; 60], Mode::Create, &[], 100).unwrap();
        assert!(write_file(server.path(), "a", &[0; 41], Mode::Update, &[], 100).is_err());
        assert_eq!(read_file(server.path(), "a").unwrap().len(), 40);
    }
}

#[derive(Deserialize)]
pub struct DirectoryQuery {
    #[serde(default = "root_directory")]
    directory: String,
}

fn root_directory() -> String {
    "/".into()
}

fn open_directory(directory: &FsPath, file: &str) -> io::Result<Dir> {
    let mut dir = Dir::open_ambient_dir(directory, ambient_authority())?;
    if !file.trim_matches('/').is_empty() {
        for component in path(file)?.components() {
            dir = dir.open_dir_nofollow(component.as_os_str())?;
        }
    }
    Ok(dir)
}

fn mime(dir: &Dir, name: &FsPath, meta: &cap_std::fs::Metadata) -> String {
    if meta.is_dir() {
        return "inode/directory".into();
    }
    if !meta.is_file() {
        return "application/octet-stream".into();
    }
    if meta.len() == 0 {
        return "inode/x-empty".into();
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .follow(FollowSymlinks::No);
    let mut content = Vec::new();
    if let Ok(file) = dir.open_with(name, &options)
        && file.take(512).read_to_end(&mut content).is_ok()
    {
        if content.starts_with(b"PK\x03\x04") {
            return if name.extension().is_some_and(|e| e == "jar") {
                "application/jar"
            } else {
                "application/zip"
            }
            .into();
        }
        if content.starts_with(&[0x1f, 0x8b]) {
            return "application/gzip".into();
        }
        if !content.contains(&0) && std::str::from_utf8(&content).is_ok() {
            return "text/plain".into();
        }
    }
    "application/octet-stream".into()
}

fn permissions(meta: &cap_std::fs::Metadata) -> String {
    use cap_std::fs::PermissionsExt;
    let bits = meta.permissions().mode();
    let mut mode = String::from(if meta.is_dir() {
        "d"
    } else if meta.is_symlink() {
        "l"
    } else {
        "-"
    });
    for (bit, character) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        mode.push(if bits & bit != 0 { character } else { '-' });
    }
    mode
}

fn list_files(directory: &FsPath, location: &str) -> io::Result<Vec<serde_json::Value>> {
    use cap_std::fs::PermissionsExt;
    let dir = open_directory(directory, location)?;
    let mut files = Vec::new();
    for entry in dir.entries()? {
        if files.len() >= 10_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Directory exceeds the listing limit",
            ));
        }
        let entry = entry?;
        let name = entry.file_name();
        let meta = entry.metadata()?;
        let modified: chrono::DateTime<chrono::Utc> = meta.modified()?.into_std().into();
        let created: chrono::DateTime<chrono::Utc> =
            meta.created().unwrap_or(meta.modified()?).into_std().into();
        files.push(serde_json::json!({
            "name":name.to_string_lossy(), "mode":permissions(&meta),
            "mode_bits":format!("{:04o}",meta.permissions().mode() & 0o777),
            "size":meta.len(), "file":!meta.is_dir(), "symlink":meta.is_symlink(),
            "mime":mime(&dir, FsPath::new(&name), &meta), "created":created.to_rfc3339(), "modified":modified.to_rfc3339()
        }));
    }
    files.sort_by(|a, b| {
        a["file"]
            .as_bool()
            .cmp(&b["file"].as_bool())
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(files)
}

pub async fn list(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Query(query): Query<DirectoryQuery>,
) -> ApiResult<axum::Json<Vec<serde_json::Value>>> {
    state.server(&id).await?;
    let directory = state.engine.directory(&id)?;
    let files = tokio::task::spawn_blocking(move || list_files(&directory, &query.directory))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(error)?;
    Ok(axum::Json(files))
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    #[test]
    fn lists_editable_files_and_folders_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("server.properties"), "server-port=25575").unwrap();
        std::fs::write(root.path().join("binary"), [0, 1, 2, 3]).unwrap();
        std::fs::create_dir(root.path().join("config")).unwrap();
        std::fs::write(root.path().join("config/test.yml"), "setting: true").unwrap();
        std::fs::write(outside.path().join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape-dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            root.path().join("escape-file"),
        )
        .unwrap();
        let files = list_files(root.path(), "/").unwrap();
        assert_eq!(files[0]["name"], "config");
        assert_eq!(files[0]["file"], false);
        assert_eq!(
            files
                .iter()
                .find(|f| f["name"] == "server.properties")
                .unwrap()["mime"],
            "text/plain"
        );
        assert_eq!(
            files.iter().find(|f| f["name"] == "binary").unwrap()["mime"],
            "application/octet-stream"
        );
        assert_eq!(
            files.iter().find(|f| f["name"] == "escape-file").unwrap()["symlink"],
            true
        );
        assert!(list_files(root.path(), "escape-dir").is_err());
        assert!(list_files(root.path(), "../").is_err());
        assert_eq!(
            list_files(root.path(), "/config").unwrap()[0]["name"],
            "test.yml"
        );
    }
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn request(root: &str, files: &[&str]) -> DeleteRequest {
        DeleteRequest {
            root: root.into(),
            files: files.iter().map(|f| (*f).into()).collect(),
        }
    }

    #[test]
    fn deletes_files_bulk_folders_and_overlapping_selections() {
        let server = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(server.path().join("folder/nested")).unwrap();
        std::fs::write(server.path().join("folder/nested/file"), "temporary").unwrap();
        std::fs::write(server.path().join("first"), "temporary").unwrap();
        std::fs::write(server.path().join("second"), "temporary").unwrap();
        std::fs::write(server.path().join("retain"), "unchanged").unwrap();
        delete_files(
            server.path(),
            request(
                "/",
                &[
                    "first",
                    "second",
                    "folder",
                    "folder/nested",
                    "first",
                    "missing",
                ],
            ),
            &[],
        )
        .unwrap();
        assert!(!server.path().join("folder").exists());
        assert!(!server.path().join("first").exists());
        assert!(!server.path().join("second").exists());
        assert_eq!(
            std::fs::read_to_string(server.path().join("retain")).unwrap(),
            "unchanged"
        );
        std::fs::create_dir_all(server.path().join("directory/nested")).unwrap();
        std::fs::write(server.path().join("directory/nested/file"), "temporary").unwrap();
        delete_files(server.path(), request("/directory", &["nested/file"]), &[]).unwrap();
        assert!(server.path().join("directory/nested").is_dir());
        assert!(!server.path().join("directory/nested/file").exists());
        delete_files(server.path(), request("/missing", &["missing"]), &[]).unwrap();
    }

    #[test]
    fn rejects_root_traversal_and_directory_aliases_without_touching_files() {
        let server = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private"), "untouched").unwrap();
        std::fs::write(server.path().join("retain"), "untouched").unwrap();
        symlink(outside.path(), server.path().join("escape")).unwrap();
        for target in [
            "",
            ".",
            "./",
            "/",
            "../private",
            "/../private",
            "escape/private",
        ] {
            assert!(
                delete_files(server.path(), request("/", &["retain", target]), &[]).is_err(),
                "Allowed {target}"
            );
            assert!(server.path().join("retain").exists());
        }
        for root in ["../", "/../", "escape"] {
            assert!(delete_files(server.path(), request(root, &["private"]), &[]).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("private")).unwrap(),
            "untouched"
        );
        assert!(server.path().is_dir());
        assert!(delete_files(server.path(), request("/", &[]), &[]).is_err());
    }

    #[test]
    fn protected_descendants_block_the_entire_request() {
        let server = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(server.path().join("folder/nested")).unwrap();
        std::fs::write(
            server.path().join("folder/nested/protected.secret"),
            "untouched",
        )
        .unwrap();
        std::fs::write(server.path().join("folder/ordinary"), "untouched").unwrap();
        std::fs::write(server.path().join("ordinary"), "untouched").unwrap();
        let denylist = vec!["*.secret".into()];
        assert_eq!(
            delete_files(
                server.path(),
                request("/", &["ordinary", "folder"]),
                &denylist
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(server.path().join("ordinary").exists());
        assert!(server.path().join("folder/ordinary").exists());
        assert_eq!(
            std::fs::read_to_string(server.path().join("folder/nested/protected.secret")).unwrap(),
            "untouched"
        );
        assert!(
            delete_files(
                server.path(),
                request("/", &["folder"]),
                &["folder/".into()]
            )
            .is_err()
        );
        delete_files(
            server.path(),
            request("/folder/nested", &["protected.secret"]),
            &["*.secret".into(), "!folder/nested/protected.secret".into()],
        )
        .unwrap();
        assert!(
            !server
                .path()
                .join("folder/nested/protected.secret")
                .exists()
        );
    }

    #[test]
    fn symlinks_and_hardlinks_are_unlinked_without_deleting_their_targets() {
        let server = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private"), "untouched").unwrap();
        std::fs::create_dir(server.path().join("folder")).unwrap();
        symlink(outside.path(), server.path().join("folder/escape")).unwrap();
        symlink(outside.path().join("private"), server.path().join("alias")).unwrap();
        std::fs::write(server.path().join("protected"), "untouched").unwrap();
        std::fs::hard_link(
            server.path().join("protected"),
            server.path().join("hardlink"),
        )
        .unwrap();
        delete_files(
            server.path(),
            request("/", &["folder", "alias", "hardlink"]),
            &["protected".into()],
        )
        .unwrap();
        assert!(!server.path().join("folder").exists());
        assert!(std::fs::symlink_metadata(server.path().join("alias")).is_err());
        assert!(!server.path().join("hardlink").exists());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("private")).unwrap(),
            "untouched"
        );
        assert_eq!(
            std::fs::read_to_string(server.path().join("protected")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn excessive_depth_is_rejected_before_any_deletion() {
        let server = tempfile::tempdir().unwrap();
        let mut deep = server.path().join("deep");
        for _ in 0..130 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(server.path().join("ordinary"), "untouched").unwrap();
        assert_eq!(
            delete_files(server.path(), request("/", &["ordinary", "deep"]), &[])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(server.path().join("ordinary").exists());
        assert!(deep.exists());
    }
}

#[cfg(test)]
mod scan_limits_tests {
    use super::*;
    #[test]
    fn oversized_and_deep_trees_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "a").unwrap();
        let dir = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        assert!(scan_usage(&dir, 0, &mut 0).is_err());
        assert!(scan_usage(&dir, 129, &mut 100).is_err());
    }
}
