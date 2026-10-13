macro_rules! ensure {($condition:expr,$($message:tt)*)=>{if !$condition{return Err(anyhow::anyhow!($($message)*).into());}}}
use crate::{
    State,
    api::{ApiError, ApiResult},
    sandbox::Sandbox,
};
use anyhow::{Context, Result};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State as AppState},
    http::{StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
#[derive(Serialize, Deserialize)]
struct Record {
    server: String,
    adapter: String,
    checksum: String,
    size: u64,
}
#[derive(Deserialize)]
pub struct BackupRequest {
    uuid: String,
    adapter: String,
    #[serde(default)]
    ignore: String,
}
fn path(state: &State, id: &str, extension: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    let dir = state.engine.config.private_root().join("backups");
    crate::sandbox::private_directory(&dir)?;
    Ok(dir.join(format!("{id}.{extension}")))
}
pub fn digest(path: &std::path::Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        hash.update(&b[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn trusted_sandbox(state: &State, id: &str, settings: &Value) -> Result<Sandbox> {
    let mut settings = settings.clone();
    settings["egg"]["file_denylist"] = json!([]);
    Sandbox::new(
        &state.engine.directory(id)?,
        &settings,
        &state.engine.config,
    )
}
pub async fn create(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<BackupRequest>,
) -> ApiResult<StatusCode> {
    ensure!(
        matches!(body.adapter.as_str(), "wings" | "s3"),
        "Unknown backup adapter"
    );
    let target = path(&state, &body.uuid, "tar.gz")?;
    let metadata = path(&state, &body.uuid, "json")?;
    ensure!(
        !target.try_exists()? && !metadata.try_exists()?,
        "Backup already exists"
    );
    let server = state.server(&id).await?;
    let lock = server
        .operation
        .clone()
        .try_lock_owned()
        .map_err(|_| ApiError(StatusCode::CONFLICT, "Server is busy.".into()))?;
    ensure!(
        !server.lifecycle.lock().await.transferring,
        "Server is being transferred"
    );
    ensure!(
        !server.installing.load(std::sync::atomic::Ordering::SeqCst),
        "Server is installing"
    );
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    let settings = server.settings.read().await.clone();
    let sb = trusted_sandbox(&state, &id, &settings)?;
    let private = state.engine.config.private_root();
    let transfer_limit = state.engine.config.limits.transfer_bytes;
    tokio::spawn(async move {
        let _lock = lock;
        let _permit = permit;
        let result=async{let work=tempfile::Builder::new().prefix("backup-").tempdir_in(&private)?;let archive=work.path().join("backup.tar.gz");let destination=archive.clone();let ignore=body.ignore.clone();tokio::task::spawn_blocking(move||crate::archives::pack(&sb,"/",&[],&destination,&ignore)).await??;
   let size=std::fs::metadata(&archive)?.len();ensure!(size<=transfer_limit,"Backup transfer limit exceeded");let checksum=tokio::task::spawn_blocking({let a=archive.clone();move||digest(&a)}).await??;
   let mut parts=Vec::new();
   if body.adapter=="s3" {
    let urls=state.remote(&format!("backups/{}?size={size}",body.uuid)).await?;let urls_list=urls["parts"].as_array().context("Missing S3 upload parts")?;let part_size=urls["part_size"].as_u64().context("Missing S3 part size")?;ensure!(part_size>0&&part_size<=5*1024*1024*1024&&urls_list.len()<=10000&&urls_list.len()as u64==size.div_ceil(part_size),"Invalid S3 multipart plan");
    let config=state.config.read().await.clone();let mut offset=0;
    for(i,url)in urls_list.iter().enumerate(){let count=(size-offset).min(part_size);let(client,url)=crate::network::client(url.as_str().context("Invalid upload URL")?,&config).await?;let mut f=tokio::fs::File::open(&archive).await?;use tokio::io::{AsyncSeekExt,SeekFrom};f.seek(SeekFrom::Start(offset)).await?;
     let response=client.put(url).header(header::CONTENT_LENGTH,count).body(reqwest::Body::wrap_stream(ReaderStream::new(f.take(count)))).send().await?.error_for_status()?;ensure!(response.status().is_success(),"S3 upload redirect rejected");let etag=response.headers().get("etag").context("Missing S3 ETag")?.to_str()?.to_owned();ensure!(etag.len()<=512,"S3 ETag too large");parts.push(json!({"etag":etag,"part_number":i+1}));offset+=count;
    }
   }
   let record=Record{server:id.clone(),adapter:body.adapter.clone(),checksum:checksum.clone(),size};
   let json=serde_json::to_vec(&record)?;
   let mut f=std::fs::OpenOptions::new().create_new(true).write(true).open(&metadata)?;f.write_all(&json)?;f.sync_all()?;
   if body.adapter=="wings" {std::fs::rename(&archive,&target)?;}
   state.remote_request(reqwest::Method::POST,&format!("backups/{}",body.uuid),Some(json!({"successful":true,"checksum_type":"sha256","checksum":checksum,"size":size,"parts":parts}))).await?;
   Ok::<(),anyhow::Error>(())
  }.await;
        if result.is_err() {
            let _=state.remote_request(reqwest::Method::POST,&format!("backups/{}",body.uuid),Some(json!({"successful":false,"size":0,"checksum":"","checksum_type":"sha256","parts":[]}))).await;
            tracing::warn!(server=%id,"Backup failed; inspect node configuration and storage");
        }
        server.emit(
            "backup completed",
            vec![json!({"uuid":body.uuid,"is_successful":result.is_ok()}).to_string()],
        );
    });
    Ok(StatusCode::ACCEPTED)
}
fn owned(state: &State, server: &str, backup: &str) -> Result<Record> {
    let p = path(state, backup, "json")?;
    ensure!(
        !p.symlink_metadata()?.is_symlink(),
        "Backup record alias rejected"
    );
    let bytes = std::fs::read(p)?;
    ensure!(bytes.len() <= 4096, "Invalid backup record");
    let record: Record = serde_json::from_slice(&bytes)?;
    ensure!(record.server == server, "Backup belongs to another server");
    Ok(record)
}
pub async fn delete(
    AppState(state): AppState<Arc<State>>,
    Path((id, backup)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    state.server(&id).await?;
    let server = state.server(&id).await?;
    let _lock = server
        .operation
        .clone()
        .try_lock_owned()
        .map_err(|_| ApiError(StatusCode::CONFLICT, "Server is busy.".into()))?;
    let record = owned(&state, &id, &backup)?;
    if record.adapter == "wings" {
        std::fs::remove_file(path(&state, &backup, "tar.gz")?)?;
    }
    std::fs::remove_file(path(&state, &backup, "json")?)?;
    Ok(StatusCode::NO_CONTENT)
}
pub async fn download(
    AppState(state): AppState<Arc<State>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let claims = crate::tokens::verify(
        &state,
        crate::tokens::query_token(query.get("token"))?,
        "backup-download",
        true,
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "Backup authorization failed.".into(),
        )
    })?;
    let record = owned(&state, &claims.server_uuid, &claims.backup_uuid)?;
    ensure!(record.adapter == "wings", "Backup is stored remotely");
    let file = tokio::fs::File::open(path(&state, &claims.backup_uuid, "tar.gz")?).await?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(header::CONTENT_LENGTH, record.size)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.tar.gz\"", claims.backup_uuid),
        )
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(anyhow::Error::from)?)
}
#[derive(Deserialize)]
pub struct Restore {
    adapter: String,
    #[serde(default)]
    truncate_directory: bool,
    #[serde(default)]
    download_url: String,
}
pub async fn restore(
    AppState(state): AppState<Arc<State>>,
    Path((id, backup)): Path<(String, String)>,
    Json(body): Json<Restore>,
) -> ApiResult<StatusCode> {
    uuid::Uuid::parse_str(&backup)?;
    ensure!(
        matches!(body.adapter.as_str(), "wings" | "s3"),
        "Unknown backup adapter"
    );
    let server = state.server(&id).await?;
    let lock = server
        .operation
        .clone()
        .try_lock_owned()
        .map_err(|_| ApiError(StatusCode::CONFLICT, "Server is busy.".into()))?;
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    ensure!(
        !server.lifecycle.lock().await.transferring,
        "Server is being transferred"
    );
    let record = if body.adapter == "wings" {
        Some(owned(&state, &id, &backup)?)
    } else {
        None
    };
    let config = state.config.read().await.clone();
    let remote = if body.adapter == "s3" {
        Some(crate::network::client(&body.download_url, &config).await?)
    } else {
        None
    };
    tokio::spawn(async move {
        let _lock = lock;
        let _permit = permit;
        let result = async {
            let work = tempfile::Builder::new()
                .prefix("restore-")
                .tempdir_in(config.private_root())?;
            let source = if let Some((client, url)) = remote {
                let response = client.get(url).send().await?.error_for_status()?;
                ensure!(response.status().is_success(), "Backup redirect rejected");
                let source = work.path().join("remote.tar.gz");
                crate::transfers::receive_file(response, &source, config.limits.transfer_bytes)
                    .await?;
                source
            } else {
                path(&state, &backup, "tar.gz")?
            };
            if let Some(record) = record {
                ensure!(
                    digest(&source)? == record.checksum,
                    "Backup checksum mismatch"
                );
            }
            let settings = server.settings.read().await.clone();
            let sb = trusted_sandbox(&state, &id, &settings)?;
            let private = config.private_root();
            let quota = sb.quota;
            let staged = tokio::task::spawn_blocking(move || {
                crate::archives::stage(&source, &private, quota)
            })
            .await??;
            crate::lifecycle::stop(&state, &server, &id, false).await?;
            let directory = state.engine.directory(&id)?;
            let root = config.private_root();
            let recovery_directory=work.keep();
            let rollback = recovery_directory.join("rollback.tar.gz");

            let restored=tokio::task::spawn_blocking(move || -> Result<()> {
                crate::archives::pack(&sb, "/", &[], &rollback, "")?;
                let result = (|| -> Result<()> {
                    if body.truncate_directory {
                        clear(&directory)?;
                    }
                    crate::archives::apply(&staged, &sb, "/", false)
                })();
                if result.is_err() {
                    clear(&directory)?;
                    let previous = crate::archives::stage(&rollback, &root, sb.quota)?;
                    crate::archives::apply(&previous, &sb, "/", false)?;
                }
                result
            })
            .await?;
            if restored.is_ok(){std::fs::remove_dir_all(recovery_directory)?;}
            else{tracing::warn!(server=%id,"Restore recovery archive retained in the private node directory");}
            restored?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let _ = state
            .remote_request(
                reqwest::Method::POST,
                &format!("backups/{backup}/restore"),
                Some(json!({"successful":result.is_ok()})),
            )
            .await;
        server.emit(
            "backup restore completed",
            vec![if result.is_ok() {
                "".into()
            } else {
                "Restore failed; existing files were retained or rolled back.".into()
            }],
        );
        if result.is_err() {
            tracing::warn!(server=%id,"Backup restoration failed");
        }
    });
    Ok(StatusCode::ACCEPTED)
}
pub fn clear(directory: &std::path::Path) -> Result<()> {
    let root = cap_std::fs::Dir::open_ambient_dir(directory, cap_std::ambient_authority())?;
    let files = root
        .entries()?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().to_string()))
        .collect::<std::io::Result<Vec<_>>>()?;
    crate::files::delete_files(
        directory,
        crate::files::DeleteRequest {
            root: "/".into(),
            files,
        },
        &[],
    )?;
    Ok(())
}
