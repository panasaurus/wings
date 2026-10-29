macro_rules! ensure {($condition:expr,$($message:tt)*)=>{if !$condition{return Err(anyhow::anyhow!($($message)*).into());}}}
use crate::{
    Server, State,
    api::{ApiError, ApiResult},
};
use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Multipart, Path, State as AppState},
    http::{HeaderMap, StatusCode},
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::{io::ReaderStream, sync::CancellationToken};
pub async fn receive_file(
    mut response: reqwest::Response,
    path: &std::path::Path,
    limit: u64,
) -> Result<u64> {
    ensure!(
        response.content_length().unwrap_or(0) <= limit,
        "Transfer exceeds size limit"
    );
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    let mut size = 0u64;
    while let Some(chunk) = response.chunk().await? {
        size = size
            .checked_add(chunk.len() as u64)
            .context("Transfer overflow")?;
        ensure!(size <= limit, "Transfer exceeds limit");
        f.write_all(&chunk).await?;
    }
    f.sync_all().await?;
    Ok(size)
}
#[derive(Deserialize)]
pub struct Outgoing {
    url: String,
    token: String,
    #[serde(default)]
    server_id: Option<String>,
}
pub async fn send(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    Json(body): Json<Outgoing>,
) -> ApiResult<StatusCode> {
    ensure!(
        body.server_id.as_ref().is_none_or(|s| s == &id),
        "Transfer server mismatch"
    );
    ensure!(
        body.token.starts_with("Bearer ") && body.token.len() <= 16 * 1024,
        "Invalid transfer token"
    );
    let config = state.config.read().await.clone();
    let (client, url) = crate::network::client(&body.url, &config).await?;
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
    let cancel = CancellationToken::new();
    {
        let mut transfers = state.transfers.lock().await;
        ensure!(!transfers.contains_key(&id), "Transfer already exists");
        transfers.insert(id.clone(), cancel.clone());
    }
    server.lifecycle.lock().await.transferring = true;
    server.emit("transfer status", vec!["starting".into()]);
    server.emit(
        "transfer logs",
        vec!["Preparing the server archive.".into()],
    );
    tokio::spawn(async move {
        let _permit = permit;
        let result=async{
  crate::lifecycle::stop(&state,&server,&id,false).await?;
  let work=tempfile::Builder::new().prefix("transfer-").tempdir_in(config.private_root())?;let archive=work.path().join("archive.tar.gz");let mut settings=server.settings.read().await.clone();settings["egg"]["file_denylist"]=json!([]);let mut trusted=settings.clone();trusted["egg"]["file_denylist"]=json!([]);let sb=crate::sandbox::Sandbox::new(&state.engine.directory(&id)?,&trusted,&config)?;let dest=archive.clone();tokio::task::spawn_blocking(move||crate::archives::pack(&sb,"/",&[],&dest,"")).await??;
  ensure!(std::fs::metadata(&archive)?.len()<=config.limits.transfer_bytes,"Transfer size limit exceeded");let checksum=crate::backup::digest(&archive)?;

  drop(lock);server.emit("transfer logs",vec!["Uploading the server archive to the destination.".into()]);
  let file=tokio::fs::File::open(&archive).await?;let size=file.metadata().await?.len();let progress=server.clone();let mut sent=0u64;let mut next=0u64;let stream=ReaderStream::with_capacity(file,65536).inspect_ok(move|chunk|{sent+=chunk.len() as u64;if sent>=next||sent==size{next=sent+1024*1024;progress.emit("transfer logs",vec![format!("Sent {sent} of {size} bytes.")]);}});let part=reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream),size).file_name("archive.tar.gz");let form=reqwest::multipart::Form::new().part("archive",part).text("checksum",checksum);
  let request=client.post(url).header("Authorization",body.token).multipart(form).send();let response=tokio::select!{_=cancel.cancelled()=>anyhow::bail!("Transfer cancelled"),r=request=>r?};ensure!(response.status()==StatusCode::OK,"Destination did not accept transfer");Ok::<(),anyhow::Error>(())
 }.await;
        state.transfers.lock().await.remove(&id);
        server.lifecycle.lock().await.transferring = false;
        if result.is_err() {
            let _ = state
                .remote_request(
                    reqwest::Method::POST,
                    &format!("servers/{id}/transfer/failure"),
                    Some(json!({})),
                )
                .await;
            server.emit("transfer status", vec!["failed".into()]);
            tracing::warn!(server=%id,"Server transfer failed");
        } else {
            server.emit("transfer status", vec!["completed".into()]);
        }
    });
    Ok(StatusCode::ACCEPTED)
}
pub async fn receive(
    AppState(state): AppState<Arc<State>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .context("Transfer authorization missing")?;
    let claims = crate::tokens::verify(&state, token, "transfer", true)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "Transfer authorization failed.".into(),
            )
        })?;
    let id = claims.sub;
    let config = state.config.read().await.clone();
    let permit = state.jobs.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Node job limit reached.".into(),
        )
    })?;
    ensure!(
        !state.servers.read().await.contains_key(&id) && !config.system.data.join(&id).exists(),
        "Destination server already exists"
    );
    let cancel = CancellationToken::new();
    {
        let mut transfers = state.transfers.lock().await;
        ensure!(!transfers.contains_key(&id), "Transfer already in progress");
        transfers.insert(id.clone(), cancel.clone());
    }
    let mut provisioned = false;
    let mut committed = false;
    let result=async{
  let work=tempfile::Builder::new().prefix("incoming-").tempdir_in(config.private_root())?;let archive=work.path().join("archive.tar.gz");let mut received=false;let mut checksum=None;
  while let Some(mut field)=multipart.next_field().await.map_err(anyhow::Error::from)?{
   match field.name().unwrap_or(""){
    "archive"=>{ensure!(!received&&checksum.is_none(),"Duplicate or out-of-order archive");let mut output=tokio::fs::OpenOptions::new().write(true).create_new(true).open(&archive).await?;let mut size=0u64;
     loop{let chunk=tokio::select!{_=cancel.cancelled()=>anyhow::bail!("Transfer cancelled"),r=field.chunk()=>r.map_err(anyhow::Error::from)?};let Some(chunk)=chunk else{break};size=size.checked_add(chunk.len()as u64).context("Transfer overflow")?;ensure!(size<=config.limits.transfer_bytes,"Transfer exceeds limit");output.write_all(&chunk).await?;}output.sync_all().await?;received=true;
    },"checksum"=>{ensure!(received&&checksum.is_none(),"Checksum must follow the archive");let mut value=Vec::new();while let Some(chunk)=field.chunk().await.map_err(anyhow::Error::from)?{ensure!(value.len()+chunk.len()<=64,"Invalid checksum length");value.extend_from_slice(&chunk);}ensure!(value.len()==64,"Invalid checksum length");checksum=Some(String::from_utf8(value)?);},_=>anyhow::bail!("Unexpected transfer field")
   }
  }
  ensure!(received&&checksum.context("Checksum missing")?==crate::backup::digest(&archive)?,"Transfer checksum mismatch");
  let response=state.remote(&format!("servers/{id}")).await?;let mut settings=response["settings"].clone();ensure!(settings["uuid"]==id,"Panel server mismatch");settings["_process_configuration"]=response["process_configuration"].clone();let quota=crate::storage::limit(&config,&settings)?;
  let staged=crate::archives::stage(&archive,&config.private_root(),quota)?;
  provisioned=true;crate::storage::provision(&config,&settings).await?;
  let mut trusted=settings.clone();trusted["egg"]["file_denylist"]=json!([]);let sb=crate::sandbox::Sandbox::new(&state.engine.directory(&id)?,&trusted,&config)?;tokio::task::spawn_blocking(move||crate::archives::apply(&staged,&sb,"/",false)).await??;
  state.engine.create(&settings).await?;state.servers.write().await.insert(id.clone(),Arc::new(Server::new(settings)));
  ensure!(!cancel.is_cancelled(),"Transfer cancelled");committed=true;
  state.remote_request(reqwest::Method::POST,&format!("servers/{id}/transfer/success"),Some(json!({}))).await?;Ok::<(),anyhow::Error>(())
 }.await;
    drop(permit);
    state.transfers.lock().await.remove(&id);
    if result.is_err() {
        if provisioned && !committed {
            state.servers.write().await.remove(&id);
            let _ = state.engine.remove(&id).await;
            if crate::storage::destroy(&config, &id).await.is_err() {
                tracing::warn!(server=%id,"Failed transfer storage retained for operator cleanup");
            }
        }
        if !committed {
            let _ = state
                .remote_request(
                    reqwest::Method::POST,
                    &format!("servers/{id}/transfer/failure"),
                    Some(json!({})),
                )
                .await;
        } else {
            tracing::warn!(server=%id,"Transferred data retained because panel confirmation is uncertain");
        }
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Transfer failed validation or requires panel reconciliation.".into(),
        ));
    }
    Ok(Json(json!({})))
}
pub async fn cancel(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if let Some(token) = state.transfers.lock().await.get(&id) {
        token.cancel();
    } else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Transfer not found.".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}
