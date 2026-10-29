macro_rules! ensure {($condition:expr,$($message:tt)*)=>{if !$condition{return Err(anyhow::anyhow!($($message)*).into());}}}
use crate::{
    State,
    api::{ApiError, ApiResult},
    sandbox::{Sandbox, relative},
};
use anyhow::{Context, Result};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State as AppState},
    http::{StatusCode, header},
    response::Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, io::Read, sync::Arc};
use tokio::io::AsyncWriteExt;
use tokio_util::{io::ReaderStream, sync::CancellationToken};

pub async fn sandbox(state: &State, id: &str) -> Result<Sandbox> {
    let server = state.server(id).await?;
    let settings = server.settings.read().await.clone();
    Sandbox::new(
        &state.engine.directory(id)?,
        &settings,
        &state.engine.config,
    )
}
pub async fn mutate<T: Send + 'static>(
    state: Arc<State>,
    id: String,
    job: impl FnOnce(Sandbox) -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    let server = state.server(&id).await?;
    let lock = server.operation.clone().try_lock_owned().map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "A server operation is already running.".into(),
        )
    })?;
    ensure!(
        !server.installing.load(std::sync::atomic::Ordering::SeqCst),
        "Server is installing"
    );
    ensure!(
        !server.lifecycle.lock().await.transferring,
        "Server is being transferred"
    );
    let sb = sandbox(&state, &id).await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let _permit = permit;
        job(sb)
    })
    .await
    .map_err(anyhow::Error::from)??)
}
fn join(root: &str, name: &str) -> Result<String> {
    let root = relative(root, true)?;
    let name = relative(name, false)?;
    Ok(root.join(name).to_str().context("Invalid path")?.into())
}
#[derive(Deserialize)]
pub struct Directory {
    #[serde(default)]
    root: String,
    name: String,
}
pub async fn mkdir(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Directory>,
) -> ApiResult<StatusCode> {
    let target = join(&body.root, &body.name)?;
    mutate(state, id, move |sb| sb.mkdir(&target)).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub struct Rename {
    #[serde(default)]
    root: String,
    files: Vec<RenamePair>,
}
#[derive(Deserialize)]
struct RenamePair {
    from: String,
    to: String,
}
pub async fn rename(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Rename>,
) -> ApiResult<StatusCode> {
    ensure!(
        !body.files.is_empty() && body.files.len() <= 1000,
        "Invalid rename batch"
    );
    let pairs = body
        .files
        .iter()
        .map(|p| Ok((join(&body.root, &p.from)?, join(&body.root, &p.to)?)))
        .collect::<Result<Vec<_>>>()?;
    mutate(state, id, move |sb| {
        for (a, b) in &pairs {
            sb.checked(a, false)?;
            sb.checked(b, false)?;
            sb.validate_tree(&relative(a, false)?)?;
        }
        for (a, b) in pairs {
            sb.rename(&a, &b)?;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub struct Copy {
    location: String,
}
pub async fn copy(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Copy>,
) -> ApiResult<StatusCode> {
    mutate(state, id, move |sb| {
        let p = sb.checked(&body.location, false)?;
        let name = p.file_name().context("Missing file")?.to_string_lossy();
        let stem = p.file_stem().unwrap_or_default().to_string_lossy();
        let extension = p
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let parent = p.parent().unwrap_or(std::path::Path::new(""));
        for index in 1..1000 {
            let target = parent.join(format!("{stem} copy {index}{extension}"));
            if sb
                .root
                .symlink_metadata(&target)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return sb.copy(&body.location, target.to_str().context("Invalid filename")?);
            }
        }
        anyhow::bail!("No copy name available for {name}")
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub struct Chmod {
    #[serde(default)]
    root: String,
    files: Vec<ChmodEntry>,
}
#[derive(Deserialize)]
struct ChmodEntry {
    file: String,
    mode: String,
}
pub async fn chmod(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Chmod>,
) -> ApiResult<StatusCode> {
    ensure!(
        !body.files.is_empty() && body.files.len() <= 1000,
        "Invalid chmod batch"
    );
    let entries = body
        .files
        .iter()
        .map(|e| Ok((join(&body.root, &e.file)?, u32::from_str_radix(&e.mode, 8)?)))
        .collect::<Result<Vec<_>>>()?;
    mutate(state, id, move |sb| {
        for (file, mode) in &entries {
            ensure!(*mode & !0o777 == 0, "Special permission bits forbidden");
            sb.open(file, false, false, false, false)?;
        }
        for (file, mode) in entries {
            sb.chmod(&file, mode)?;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub struct Selection {
    #[serde(default)]
    root: String,
    files: Vec<String>,
}
pub async fn compress(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Selection>,
) -> ApiResult<Json<Value>> {
    ensure!(!body.files.is_empty(), "Choose files to archive");
    let private = state.engine.config.private_root();
    let value=mutate(state,id,move|sb|{let job=tempfile::Builder::new().prefix("compress-").tempdir_in(private)?;let archive=job.path().join("archive.tar.gz");crate::archives::pack(&sb,&body.root,&body.files,&archive,"")?;let name=format!("archive-{}.tar.gz",uuid::Uuid::new_v4());let size=sb.write(&join(&body.root,&name)?,&mut std::fs::File::open(archive)?,sb.quota,true)?;Ok(json!({"name":name,"size":size,"is_file":true,"is_symlink":false,"mimetype":"application/gzip","mode":"rw-r--r--","mode_bits":"0644","created_at":chrono::Utc::now().to_rfc3339(),"modified_at":chrono::Utc::now().to_rfc3339()}))}).await?;
    Ok(Json(value))
}
#[derive(Deserialize)]
pub struct Decompress {
    #[serde(default)]
    root: String,
    file: String,
}
pub async fn decompress(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Decompress>,
) -> ApiResult<StatusCode> {
    let private = state.engine.config.private_root();
    mutate(state, id, move |sb| {
        let mut temp = tempfile::NamedTempFile::new_in(&private)?;
        std::io::copy(
            &mut sb
                .open(&join(&body.root, &body.file)?, false, false, false, false)?
                .take(sb.quota.saturating_add(1)),
            &mut temp,
        )?;
        let staged = crate::archives::stage(temp.path(), &private, sb.quota)?;
        crate::archives::apply(&staged, &sb, &body.root, true)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn download(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let claims = crate::tokens::verify(
        &state,
        crate::tokens::query_token(query.get("token"))?,
        "file-download",
        true,
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "Download authorization failed.".into(),
        )
    })?;
    let sb = sandbox(&state, &claims.server_uuid).await?;
    let file = sb.open(&claims.file_path, false, false, false, false)?;
    let size = file.metadata()?.len();
    let filename = std::path::Path::new(&claims.file_path)
        .file_name()
        .context("Invalid filename")?
        .to_string_lossy();
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " ._-".contains(*c))
        .collect();
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(
        file.into_std(),
    )));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                if safe.is_empty() { "download" } else { &safe }
            ),
        )
        .header("x-content-type-options", "nosniff")
        .body(body)
        .map_err(anyhow::Error::from)?;
    state
        .audit(
            "server:file.download",
            &claims.server_uuid,
            &claims.user_uuid,
            "",
            json!({"file":claims.file_path}),
        )
        .await;
    Ok(response)
}
async fn upload_inner(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> ApiResult<StatusCode> {
    let claims = crate::tokens::verify(
        &state,
        crate::tokens::query_token(query.get("token"))?,
        "file-upload",
        true,
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "Upload authorization failed.".into(),
        )
    })?;
    let server = state.server(&claims.server_uuid).await?;
    let lock = server
        .operation
        .clone()
        .try_lock_owned()
        .map_err(|_| ApiError(StatusCode::CONFLICT, "Server is busy.".into()))?;
    ensure!(
        !server.installing.load(std::sync::atomic::Ordering::SeqCst),
        "Server is installing"
    );
    ensure!(
        !server.lifecycle.lock().await.transferring,
        "Server is being transferred"
    );
    let _permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    ensure!(
        claims.permissions.is_empty()
            || claims
                .permissions
                .iter()
                .any(|p| p == "file.create" || p == "*"),
        "File creation permission missing"
    );
    let config = state.config.read().await.clone();
    let limit = config.api.upload_limit * 1024 * 1024;
    let directory = query.get("directory").map(String::as_str).unwrap_or("/");
    let sb = sandbox(&state, &claims.server_uuid).await?;
    sb.checked(directory, true)?;
    let mut bytes = 0u64;
    let mut files = 0usize;
    let exclusive = !claims
        .permissions
        .iter()
        .any(|p| p == "file.update" || p == "*");
    while let Some(mut field) = multipart.next_field().await.map_err(anyhow::Error::from)? {
        ensure!(files < 1000, "Too many upload files");
        let name = field
            .file_name()
            .context("Upload filename missing")?
            .to_owned();
        ensure!(
            relative(&name, false)?.components().count() == 1,
            "Upload filename must be a basename"
        );
        let target = join(directory, &name)?;
        let temp = tempfile::NamedTempFile::new_in(config.private_root())?;
        let mut output = tokio::fs::File::from_std(temp.reopen()?);
        while let Some(chunk) = field.chunk().await.map_err(anyhow::Error::from)? {
            bytes = bytes
                .checked_add(chunk.len() as u64)
                .context("Upload size overflow")?;
            ensure!(bytes <= limit, "Upload exceeds size limit");
            output.write_all(&chunk).await?;
        }
        output.sync_all().await?;
        drop(output);
        let size = std::fs::metadata(temp.path())?.len();
        sb.write(
            &target,
            &mut std::fs::File::open(temp.path())?,
            limit,
            exclusive,
        )?;
        files += 1;
        state
            .audit(
                "server:file.upload",
                &claims.server_uuid,
                &claims.user_uuid,
                "",
                json!({"file":target,"size":size}),
            )
            .await;
    }
    drop(lock);
    ensure!(files > 0, "No upload files supplied");
    Ok(StatusCode::NO_CONTENT)
}

pub struct DownloadJob {
    pub server: String,
    pub filename: String,
    pub size: u64,
    pub downloaded: u64,
    pub status: String,
    pub cancel: CancellationToken,
    pub created: u64,
}
#[derive(Deserialize)]
pub struct Pull {
    url: String,
    #[serde(default, alias = "root")]
    directory: String,
    #[serde(default, alias = "file_name")]
    filename: Option<String>,
    #[serde(default)]
    foreground: bool,
    #[serde(default)]
    use_header: bool,
}
pub async fn pull(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Pull>,
) -> ApiResult<Json<Value>> {
    state.server(&id).await?;
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    let config = state.config.read().await.clone();
    let (response, url) = crate::network::get(&body.url, &config).await?;
    let default = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    let mut filename = body.filename.unwrap_or_else(|| default.into());
    if body.use_header
        && let Some(header) = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|h| h.to_str().ok())
        && let Some(value) = header
            .split(';')
            .find_map(|p| p.trim().strip_prefix("filename="))
    {
        filename = value.trim_matches('"').into();
    }
    ensure!(
        relative(&filename, false)?.components().count() == 1,
        "Download filename must be a basename"
    );
    let target = join(&body.directory, &filename)?;
    let sb = sandbox(&state, &id).await?;
    sb.checked(&target, false)?;
    let size = response.content_length().unwrap_or(0);
    ensure!(
        size <= sb.quota.min(config.limits.transfer_bytes),
        "Download exceeds quota"
    );
    let key = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    {
        let mut jobs = state.downloads.lock().await;
        let now = chrono::Utc::now().timestamp() as u64;
        jobs.retain(|_, j| j.status == "downloading" || j.created + 3600 > now);
        ensure!(jobs.len() < 1000, "Download history limit reached");
        jobs.insert(
            key.clone(),
            DownloadJob {
                server: id.clone(),
                filename: filename.clone(),
                size,
                downloaded: 0,
                status: "downloading".into(),
                cancel: cancel.clone(),
                created: now,
            },
        );
    }
    let run = {
        let state = state.clone();
        let key = key.clone();
        async move {
            let result=async {
  let temp=tempfile::NamedTempFile::new_in(config.private_root())?;let mut output=tokio::fs::File::from_std(temp.reopen()?);let mut response=response;let mut bytes=0u64;
  loop{let chunk=tokio::select!{_ = cancel.cancelled()=>anyhow::bail!("Download cancelled"),chunk=response.chunk()=>chunk?};let Some(chunk)=chunk else{break};bytes=bytes.checked_add(chunk.len()as u64).context("Download overflow")?;ensure!(bytes<=sb.quota.min(config.limits.transfer_bytes),"Download exceeds quota");output.write_all(&chunk).await?;if let Some(job)=state.downloads.lock().await.get_mut(&key){job.downloaded=bytes;}}
  output.sync_all().await?;drop(output);
  drop(permit);
  mutate(state.clone(),id.clone(),move|sb|sb.write(&target,&mut std::fs::File::open(temp.path())?,config.limits.transfer_bytes,true)).await.map_err(|e|anyhow::anyhow!(e.1))?;
  Ok::<(),anyhow::Error>(())
 }.await;
            let status = if result.is_ok() {
                "completed"
            } else if cancel.is_cancelled() {
                "cancelled"
            } else {
                "failed"
            };
            if let Some(job) = state.downloads.lock().await.get_mut(&key) {
                job.status = status.into();
            }
            result
        }
    };
    if body.foreground {
        run.await?;
    } else {
        tokio::spawn(async move {
            if let Err(e) = run.await {
                tracing::warn!(error=%e,"Remote download failed");
            }
        });
    }
    Ok(Json(json!({"identifier":key,"uuid":key})))
}
pub async fn pulling(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    state.server(&id).await?;
    let jobs = state.downloads.lock().await;
    Ok(Json(json!(jobs.iter().filter(|(_,j)|j.server==id).map(|(k,j)|json!({"identifier":k,"uuid":k,"file":j.filename,"size":j.size,"downloaded":j.downloaded,"status":j.status})).collect::<Vec<_>>())))
}
pub async fn cancel_pull(
    AppState(state): AppState<Arc<State>>,
    Path((id, key)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    state.server(&id).await?;
    let jobs = state.downloads.lock().await;
    let job = jobs
        .get(&key)
        .filter(|j| j.server == id)
        .context("Download not found")?;
    job.cancel.cancel();
    Ok(StatusCode::NO_CONTENT)
}

pub async fn upload(
    state: AppState<Arc<State>>,
    query: Query<HashMap<String, String>>,
    multipart: Multipart,
) -> ApiResult<StatusCode> {
    tokio::time::timeout(
        std::time::Duration::from_secs(900),
        upload_inner(state, query, multipart),
    )
    .await
    .map_err(|_| ApiError(StatusCode::REQUEST_TIMEOUT, "Upload timed out.".into()))?
}
