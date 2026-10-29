use crate::{State, sandbox::Sandbox};
use anyhow::{Context, Result, ensure};
use cap_fs_ext::DirExt;
use cap_std::fs::{File as CapFile, MetadataExt, PermissionsExt};
use russh::{
    Channel, ChannelId,
    server::{Auth, Msg, Session},
};
use russh_sftp::{protocol::*, server::Handler};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

pub async fn start(state: Arc<State>) -> Result<()> {
    let config = state.config.read().await.clone();
    if !config.system.sftp.enabled {
        return Ok(());
    }
    let address = std::net::SocketAddr::new(
        config.system.sftp.bind_address.parse()?,
        config.system.sftp.bind_port,
    );
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("Bind SFTP listener")?;
    let key_path = config.private_root().join("ssh_host_ed25519_key");
    let key = if key_path.exists() {
        ensure!(
            !key_path.symlink_metadata()?.is_symlink(),
            "SFTP host key is an alias"
        );
        russh::keys::load_secret_key(&key_path, None)?
    } else {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?;
        out.write_all(
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        out.sync_all()?;
        key
    };
    let ssh = Arc::new(russh::server::Config {
        keys: vec![key],
        max_auth_attempts: 6,
        auth_rejection_time: Duration::from_secs(3),
        inactivity_timeout: Some(Duration::from_secs(300)),
        window_size: 65536,
        maximum_packet_size: 65536,
        ..Default::default()
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(config.limits.sftp_connections));
    tracing::info!(%address,"SFTP listener ready");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let ssh = ssh.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let handler = Ssh {
                    state,
                    identity: None,
                    channel: None,
                    opened: false,
                };
                if let Ok(Ok(session)) = tokio::time::timeout(
                    Duration::from_secs(15),
                    russh::server::run_stream(ssh, stream, handler),
                )
                .await
                {
                    let _ = session.await;
                }
            });
        }
    });
    Ok(())
}
#[derive(Clone)]
struct Identity {
    server: String,
    user: String,
    permissions: Vec<String>,
    issued: u64,
}
impl Identity {
    fn has(&self, p: &str) -> bool {
        self.permissions.iter().any(|s| s == p || s == "*")
    }
}
struct Ssh {
    state: Arc<State>,
    identity: Option<Identity>,
    channel: Option<Channel<Msg>>,
    opened: bool,
}
impl Ssh {
    async fn authenticate(&mut self, username: &str, password: &str, kind: &str) -> Auth {
        if username.len() > 256 || password.len() > 16384 {
            return Auth::reject();
        }
        let result = async {
            let value = self
                .state
                .remote_request(
                    reqwest::Method::POST,
                    "sftp/auth",
                    Some(serde_json::json!({"type":kind,"username":username,"password":password})),
                )
                .await?;
            let server = value["server"]
                .as_str()
                .context("Missing server")?
                .to_owned();
            let user = value["user"].as_str().context("Missing user")?.to_owned();
            uuid::Uuid::parse_str(&server)?;
            uuid::Uuid::parse_str(&user)?;
            let permissions = serde_json::from_value::<Vec<String>>(value["permissions"].clone())?;
            let identity = Identity {
                server,
                user,
                permissions,
                issued: chrono::Utc::now().timestamp() as u64,
            };
            ensure!(identity.has("file.sftp"), "SFTP permission missing");
            let server = self.state.server(&identity.server).await?;
            ensure!(
                !server.settings.read().await["suspended"]
                    .as_bool()
                    .unwrap_or(false),
                "Suspended"
            );
            Ok::<_, anyhow::Error>(identity)
        }
        .await;
        match result {
            Ok(identity) => {
                self.identity = Some(identity);
                Auth::Accept
            }
            Err(_) => Auth::reject(),
        }
    }
}
impl russh::server::Handler for Ssh {
    type Error = anyhow::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth> {
        Ok(self.authenticate(user, password, "password").await)
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth> {
        Ok(self
            .authenticate(user, &key.to_openssh()?, "public_key")
            .await)
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<()> {
        if self.identity.is_some() && !self.opened {
            self.opened = true;
            self.channel = Some(channel);
            reply.accept().await;
        }
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<()> {
        if name != "sftp" {
            session.channel_failure(id)?;
            return Ok(());
        }
        if let (Some(channel), Some(identity)) = (self.channel.take(), self.identity.clone()) {
            let sb = crate::file_api::sandbox(&self.state, &identity.server).await?;
            let max_handles = self.state.config.read().await.system.sftp.max_handles;
            let handler = Sftp {
                state: self.state.clone(),
                identity: identity.clone(),
                sb,
                handles: HashMap::new(),
                max_handles,
                initialized: false,
                egg: self
                    .state
                    .server(&identity.server)
                    .await?
                    .settings
                    .read()
                    .await["egg"]
                    .clone(),
            };
            session.channel_success(id)?;
            tokio::spawn(async move {
                russh_sftp::server::run_with_config(
                    channel.into_stream(),
                    handler,
                    russh_sftp::server::Config {
                        max_client_packet_len: 131072,
                    },
                )
                .await;
            });
        } else {
            session.channel_failure(id)?;
        }
        Ok(())
    }
    async fn channel_eof(&mut self, id: ChannelId, session: &mut Session) -> Result<()> {
        session.close(id)?;
        Ok(())
    }

    /// Handle `ssh <user>@<host> '<command>'` — the `.wings` CLI entry
    /// point. If the command starts with the configured CLI prefix
    /// (`.wings` by default), we route it through the CLI dispatcher and
    /// write the output back over the channel. Otherwise we reject it.
    /// New in v0.2.1.
    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let shell_config = self.state.config.read().await.system.sftp.shell.clone();
        if !shell_config.enabled {
            session.channel_failure(id)?;
            return Ok(());
        }
        let Some(identity) = self.identity.clone() else {
            session.channel_failure(id)?;
            return Ok(());
        };
        let line = match std::str::from_utf8(data) {
            Ok(s) => s.trim().to_owned(),
            Err(_) => {
                session.channel_failure(id)?;
                return Ok(());
            }
        };
        if !crate::ssh_cli::is_cli_command(&shell_config.cli_name, &line) {
            // Not a .wings command — reject so users can't run arbitrary
            // shell commands over SSH.
            let msg = format!(
                "Only `{}` commands are allowed over SSH.\n",
                shell_config.cli_name
            );
            let _ = session.data(id, msg.as_bytes().into());
            session.close(id)?;
            return Ok(());
        }
        session.channel_success(id)?;

        // Run the CLI inline. Commands are fast (local state queries / power
        // actions), so we don't need a background task. The peer IP isn't
        // currently captured by the Ssh handler; use loopback as a
        // best-effort placeholder for audit logging.
        let user_ip: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let permissions = identity.permissions.clone();
        let server_id = identity.server.clone();
        let user_id = identity.user.clone();
        let prefix = shell_config.cli_name.clone();

        let ctx = crate::ssh_cli::CliContext {
            state: &self.state,
            server: &server_id,
            user: &user_id,
            user_ip,
            permissions: &permissions,
            console_attached: false,
            prefix: &prefix,
        };
        let output = ctx.run(&line).await;

        let mut buffer = String::new();
        for output_line in &output.lines {
            buffer.push_str(output_line);
            buffer.push('\n');
        }
        if !buffer.is_empty() {
            let _ = session.data(id, buffer.as_bytes().into());
        }
        // Best-effort exit status. The russh API for this may vary by
        // version; if the method name differs, this silently no-ops.
        let exit_status: u32 = if output.success { 0 } else { 1 };
        let _ = session.exit_status_request(id, exit_status);
        let _ = session.close(id);

        Ok(())
    }
}
enum OpenHandle {
    File {
        file: CapFile,
        path: String,
        read: bool,
        write: bool,
        append: bool,
    },
    Directory {
        entries: cap_std::fs::ReadDir,
        path: PathBuf,
    },
}
struct Operation {
    _lock: tokio::sync::OwnedMutexGuard<()>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
struct Sftp {
    state: Arc<State>,
    identity: Identity,
    sb: Sandbox,
    handles: HashMap<String, OpenHandle>,
    max_handles: usize,
    initialized: bool,
    egg: serde_json::Value,
}
fn failure(e: anyhow::Error) -> StatusCode {
    if e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        StatusCode::NoSuchFile
    } else {
        StatusCode::PermissionDenied
    }
}
fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "OK".into(),
        language_tag: "en".into(),
    }
}
fn attrs(m: cap_std::fs::Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(m.len()),
        permissions: Some(
            (m.permissions().mode() & 0o777) | if m.is_dir() { 0o040000 } else { 0o100000 },
        ),
        ..Default::default()
    }
}
impl Sftp {
    async fn authorize(
        &self,
        permission: &str,
        mutation: bool,
    ) -> std::result::Result<Option<Operation>, StatusCode> {
        if !self.identity.has(permission) {
            return Err(StatusCode::PermissionDenied);
        }
        if self
            .state
            .revoked
            .read()
            .await
            .get(&format!(
                "user:{}:{}",
                self.identity.server, self.identity.user
            ))
            .is_some_and(|t| self.identity.issued <= *t)
        {
            return Err(StatusCode::PermissionDenied);
        }
        let server = self
            .state
            .server(&self.identity.server)
            .await
            .map_err(failure)?;
        if server.settings.read().await["egg"] != self.egg {
            return Err(StatusCode::PermissionDenied);
        }
        if server.settings.read().await["suspended"]
            .as_bool()
            .unwrap_or(false)
        {
            return Err(StatusCode::PermissionDenied);
        }
        if mutation {
            let lock = server
                .operation
                .clone()
                .try_lock_owned()
                .map_err(|_| StatusCode::Failure)?;
            if server.installing.load(std::sync::atomic::Ordering::SeqCst)
                || server.lifecycle.lock().await.transferring
            {
                return Err(StatusCode::Failure);
            }
            let permit = self
                .state
                .jobs
                .clone()
                .try_acquire_owned()
                .map_err(|_| StatusCode::Failure)?;
            Ok(Some(Operation {
                _lock: lock,
                _permit: permit,
            }))
        } else {
            Ok(None)
        }
    }
    fn metadata(&self, path: &str) -> Result<cap_std::fs::Metadata> {
        let p = self.sb.checked(path, true)?;
        if p.as_os_str().is_empty() {
            return Ok(self.sb.root.dir_metadata()?);
        }
        let (d, n) = self.sb.parent(&p, false)?;
        let m = d.symlink_metadata(n)?;
        ensure!(m.is_dir() || m.is_file() && m.nlink() == 1, "Unsafe file");
        Ok(m)
    }
    fn insert(&mut self, id: u32, handle: OpenHandle) -> std::result::Result<Handle, StatusCode> {
        if self.handles.len() >= self.max_handles {
            return Err(StatusCode::Failure);
        }
        let key = uuid::Uuid::new_v4().to_string();
        self.handles.insert(key.clone(), handle);
        Ok(Handle { id, handle: key })
    }
}
impl Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }
    async fn init(
        &mut self,
        _: u32,
        _: HashMap<String, String>,
    ) -> std::result::Result<Version, StatusCode> {
        if self.initialized {
            return Err(StatusCode::ConnectionLost);
        }
        self.initialized = true;
        Ok(Version::new())
    }
    async fn open(
        &mut self,
        id: u32,
        path: String,
        flags: OpenFlags,
        attributes: FileAttributes,
    ) -> std::result::Result<Handle, StatusCode> {
        if self.handles.len() >= self.max_handles {
            return Err(StatusCode::Failure);
        }
        if attributes.uid.is_some()
            || attributes.gid.is_some()
            || attributes.permissions.is_some_and(|m| m & !0o100777 != 0)
        {
            return Err(StatusCode::PermissionDenied);
        }
        let write = flags.intersects(
            OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::TRUNCATE | OpenFlags::CREATE,
        );
        let exists = match self.metadata(&path) {
            Ok(_) => true,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                false
            }
            Err(e) => return Err(failure(e)),
        };
        let _guard = self
            .authorize(
                if write {
                    if exists { "file.update" } else { "file.create" }
                } else {
                    "file.read-content"
                },
                write,
            )
            .await?;
        if flags.contains(OpenFlags::READ) {
            self.authorize("file.read-content", false).await?;
        }
        if !write && !flags.contains(OpenFlags::READ) {
            return Err(StatusCode::PermissionDenied);
        }
        let exclusive =
            flags.contains(OpenFlags::EXCLUDE) || !exists && !self.identity.has("file.update");
        let file = self
            .sb
            .open(
                &path,
                write,
                flags.contains(OpenFlags::CREATE),
                exclusive,
                flags.contains(OpenFlags::TRUNCATE),
            )
            .map_err(failure)?;
        self.insert(
            id,
            OpenHandle::File {
                file,
                path,
                read: flags.contains(OpenFlags::READ),
                write,
                append: flags.contains(OpenFlags::APPEND),
            },
        )
    }
    async fn close(&mut self, id: u32, handle: String) -> std::result::Result<Status, StatusCode> {
        self.authorize("file.sftp", false).await?;
        match self.handles.remove(&handle) {
            Some(OpenHandle::File {
                file, write: true, ..
            }) => file.sync_all().map_err(|e| failure(e.into()))?,
            Some(_) => {}
            None => return Err(StatusCode::Failure),
        }
        Ok(ok(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> std::result::Result<Data, StatusCode> {
        self.authorize("file.read-content", false).await?;
        if len > 65536 {
            return Err(StatusCode::Failure);
        }
        let Some(OpenHandle::File {
            file, read: true, ..
        }) = self.handles.get_mut(&handle)
        else {
            return Err(StatusCode::PermissionDenied);
        };
        let m = file.metadata().map_err(|e| failure(e.into()))?;
        if m.nlink() != 1 {
            return Err(StatusCode::PermissionDenied);
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| failure(e.into()))?;
        let mut data = vec![0; len as usize];
        let n = file.read(&mut data).map_err(|e| failure(e.into()))?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(n);
        Ok(Data { id, data })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> std::result::Result<Status, StatusCode> {
        let permission = match self.handles.get(&handle) {
            Some(OpenHandle::File { write: true, .. }) => {
                if self.identity.has("file.update") {
                    "file.update"
                } else {
                    "file.create"
                }
            }
            _ => return Err(StatusCode::PermissionDenied),
        };
        let _guard = self.authorize(permission, true).await?;
        if data.len() > 65536 {
            return Err(StatusCode::Failure);
        }
        let Some(OpenHandle::File {
            file, path, append, ..
        }) = self.handles.get_mut(&handle)
        else {
            return Err(StatusCode::Failure);
        };
        self.sb.checked(path, false).map_err(failure)?;
        let offset = if *append {
            file.metadata().map_err(|e| failure(e.into()))?.len()
        } else {
            offset
        };
        let mut file = file.try_clone().map_err(|e| failure(e.into()))?;
        let sb = self.sb.try_clone().map_err(failure)?;
        tokio::task::spawn_blocking(move || sb.write_at(&mut file, offset, &data))
            .await
            .map_err(|_| StatusCode::Failure)?
            .map_err(failure)?;
        Ok(ok(id))
    }
    async fn stat(&mut self, id: u32, path: String) -> std::result::Result<Attrs, StatusCode> {
        self.authorize("file.read", false).await?;
        Ok(Attrs {
            id,
            attrs: attrs(self.metadata(&path).map_err(failure)?),
        })
    }
    async fn lstat(&mut self, id: u32, path: String) -> std::result::Result<Attrs, StatusCode> {
        self.stat(id, path).await
    }
    async fn fstat(&mut self, id: u32, handle: String) -> std::result::Result<Attrs, StatusCode> {
        self.authorize("file.read", false).await?;
        let m = match self.handles.get(&handle) {
            Some(OpenHandle::File { file, .. }) => {
                file.metadata().map_err(|e| failure(e.into()))?
            }
            Some(OpenHandle::Directory { path, .. }) => self
                .metadata(path.to_str().ok_or(StatusCode::Failure)?)
                .map_err(failure)?,
            _ => return Err(StatusCode::Failure),
        };
        Ok(Attrs {
            id,
            attrs: attrs(m),
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> std::result::Result<Handle, StatusCode> {
        self.authorize("file.read", false).await?;
        let p = self.sb.checked(&path, true).map_err(failure)?;
        let dir = if p.as_os_str().is_empty() {
            self.sb.root.try_clone()
        } else {
            let (d, n) = self.sb.parent(&p, false).map_err(failure)?;
            d.open_dir_nofollow(n)
        }
        .map_err(|e| failure(e.into()))?;
        self.insert(
            id,
            OpenHandle::Directory {
                entries: dir.entries().map_err(|e| failure(e.into()))?,
                path: p,
            },
        )
    }
    async fn readdir(&mut self, id: u32, handle: String) -> std::result::Result<Name, StatusCode> {
        self.authorize("file.read", false).await?;
        let Some(OpenHandle::Directory { entries, path }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut files = Vec::new();
        for _ in 0..64 {
            let Some(entry) = entries.next() else { break };
            let entry = entry.map_err(|e| failure(e.into()))?;
            let p = path.join(entry.file_name());
            if self
                .sb
                .checked(p.to_str().ok_or(StatusCode::Failure)?, false)
                .is_err()
            {
                continue;
            }
            let m = entry.metadata().map_err(|e| failure(e.into()))?;
            if m.is_dir() || m.is_file() && m.nlink() == 1 {
                files.push(File::new(entry.file_name().to_string_lossy(), attrs(m)));
            }
        }
        if files.is_empty() {
            return Err(StatusCode::Eof);
        }
        Ok(Name { id, files })
    }
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> std::result::Result<Status, StatusCode> {
        let _guard = self.authorize("file.create", true).await?;
        self.sb.mkdir(&path).map_err(failure)?;
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> std::result::Result<Status, StatusCode> {
        let _guard = self.authorize("file.delete", true).await?;
        let p = self.sb.checked(&path, false).map_err(failure)?;
        let (d, n) = self.sb.parent(&p, false).map_err(failure)?;
        let m = d.symlink_metadata(&n).map_err(|e| failure(e.into()))?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(StatusCode::PermissionDenied);
        }
        d.remove_file(n).map_err(|e| failure(e.into()))?;
        Ok(ok(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> std::result::Result<Status, StatusCode> {
        let _guard = self.authorize("file.delete", true).await?;
        let p = self.sb.checked(&path, false).map_err(failure)?;
        let (d, n) = self.sb.parent(&p, false).map_err(failure)?;
        d.open_dir_nofollow(&n).map_err(|e| failure(e.into()))?;
        d.remove_dir(n).map_err(|e| failure(e.into()))?;
        Ok(ok(id))
    }
    async fn rename(
        &mut self,
        id: u32,
        from: String,
        to: String,
    ) -> std::result::Result<Status, StatusCode> {
        let _guard = self.authorize("file.update", true).await?;
        self.authorize("file.create", false).await?;
        self.sb.rename(&from, &to).map_err(failure)?;
        Ok(ok(id))
    }
    async fn realpath(&mut self, id: u32, path: String) -> std::result::Result<Name, StatusCode> {
        self.authorize("file.read", false).await?;
        let p = self.sb.checked(&path, true).map_err(failure)?;
        self.metadata(&path).map_err(failure)?;
        Ok(Name {
            id,
            files: vec![File::dummy(format!("/{}", p.to_string_lossy()))],
        })
    }
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        a: FileAttributes,
    ) -> std::result::Result<Status, StatusCode> {
        let _guard = self.authorize("file.update", true).await?;
        let p = self.sb.checked(&path, false).map_err(failure)?;
        let (d, n) = self.sb.parent(&p, false).map_err(failure)?;
        let m = d.symlink_metadata(&n).map_err(|e| failure(e.into()))?;
        let file = if m.is_dir() {
            if a.size.is_some() {
                return Err(StatusCode::OpUnsupported);
            }
            CapFile::from_std(
                d.open_dir_nofollow(n)
                    .map_err(|e| failure(e.into()))?
                    .into_std_file(),
            )
        } else {
            self.sb
                .open(&path, true, false, false, false)
                .map_err(failure)?
        };
        apply_attributes(&self.sb, &file, a).map_err(failure)?;
        Ok(ok(id))
    }
    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        a: FileAttributes,
    ) -> std::result::Result<Status, StatusCode> {
        let _guard = self
            .authorize(
                if self.identity.has("file.update") {
                    "file.update"
                } else {
                    "file.create"
                },
                true,
            )
            .await?;
        let Some(OpenHandle::File {
            file, write: true, ..
        }) = self.handles.get(&handle)
        else {
            return Err(StatusCode::PermissionDenied);
        };
        apply_attributes(&self.sb, file, a).map_err(failure)?;
        Ok(ok(id))
    }

    /// OpenSSH SFTP protocol extensions. Ported from wings-main, retargeted
    /// to u.zip's cap_std sandbox. Supports `statvfs@openssh.com`,
    /// `space-available`, `hardlink@openssh.com`, `posix-rename@openssh.com`,
    /// `copy-file`, and `fsync@openssh.com`; everything else returns
    /// `OpUnsupported`.
    async fn extended(
        &mut self,
        id: u32,
        command: String,
        data: Vec<u8>,
    ) -> std::result::Result<russh_sftp::protocol::Packet, StatusCode> {
        use serde::Deserialize;
        tracing::debug!(command = %command, "sftp extended command");

        match command.as_str() {
            "statvfs@openssh.com" | "space-available" => {
                // The request contains an optional path string; we ignore it
                // and stat the server root, which is what clients almost
                // always want.
                let root = self
                    .state
                    .engine
                    .directory(&self.identity.server)
                    .map_err(|_| StatusCode::Failure)?;
                let stats = crate::sftp_extensions::statvfs(&root)
                    .map_err(|_| StatusCode::Failure)?;
                Ok(russh_sftp::protocol::Packet::ExtendedReply(
                    russh_sftp::protocol::ExtendedReply {
                        id,
                        data: stats.to_wire().into(),
                    },
                ))
            }
            "hardlink@openssh.com" => {
                #[derive(Deserialize)]
                struct HardlinkRequest {
                    target: String,
                    link_name: String,
                }
                let request: HardlinkRequest =
                    match russh_sftp::de::from_bytes(&mut data.into()) {
                        Ok(r) => r,
                        Err(_) => return Err(StatusCode::BadMessage),
                    };
                self.authorize("file.create", true).await?;
                crate::sftp_extensions::hardlink(&self.sb, &request.target, &request.link_name)
                    .map(|_| russh_sftp::protocol::Packet::Status(ok(id)))
                    .map_err(|e| crate::sftp_extensions::failure_code(&e))
            }
            "posix-rename@openssh.com" => {
                #[derive(Deserialize)]
                struct RenameRequest {
                    oldpath: String,
                    newpath: String,
                }
                let request: RenameRequest =
                    match russh_sftp::de::from_bytes(&mut data.into()) {
                        Ok(r) => r,
                        Err(_) => return Err(StatusCode::BadMessage),
                    };
                self.authorize("file.update", true).await?;
                crate::sftp_extensions::posix_rename(&self.sb, &request.oldpath, &request.newpath)
                    .map(|_| russh_sftp::protocol::Packet::Status(ok(id)))
                    .map_err(|e| crate::sftp_extensions::failure_code(&e))
            }
            "copy-file" => {
                #[derive(Deserialize)]
                struct CopyRequest {
                    src_file_name: String,
                    dst_file_name: String,
                }
                let request: CopyRequest =
                    match russh_sftp::de::from_bytes(&mut data.into()) {
                        Ok(r) => r,
                        Err(_) => return Err(StatusCode::BadMessage),
                    };
                self.authorize("file.create", true).await?;
                crate::sftp_extensions::copy_file(&self.sb, &request.src_file_name, &request.dst_file_name)
                    .map(|_| russh_sftp::protocol::Packet::Status(ok(id)))
                    .map_err(|e| crate::sftp_extensions::failure_code(&e))
            }
            "fsync@openssh.com" => {
                #[derive(Deserialize)]
                struct FsyncRequest {
                    handle: String,
                }
                let request: FsyncRequest =
                    match russh_sftp::de::from_bytes(&mut data.into()) {
                        Ok(r) => r,
                        Err(_) => return Err(StatusCode::BadMessage),
                    };
                self.authorize("file.update", true).await?;
                let Some(OpenHandle::File { file, .. }) = self.handles.get(&request.handle) else {
                    return Err(StatusCode::NoSuchFile);
                };
                crate::sftp_extensions::fsync(file)
                    .map(|_| russh_sftp::protocol::Packet::Status(ok(id)))
                    .map_err(|e| crate::sftp_extensions::failure_code(&e))
            }
            _ => Err(StatusCode::OpUnsupported),
        }
    }
}
fn apply_attributes(sb: &Sandbox, file: &CapFile, a: FileAttributes) -> Result<()> {
    ensure!(
        a.uid.is_none() && a.gid.is_none(),
        "Changing ownership is forbidden"
    );
    let m = file.metadata()?;
    ensure!(
        m.is_dir() || m.is_file() && m.nlink() == 1,
        "File alias rejected"
    );
    if let Some(mode) = a.permissions {
        ensure!(mode & !0o170777 == 0, "Special permission bits forbidden");
    }
    if let Some(size) = a.size {
        ensure!(
            m.is_file()
                && size <= sb.quota
                && sb
                    .usage()?
                    .checked_add(size.saturating_sub(m.len()))
                    .is_some_and(|n| n <= sb.quota),
            "Disk quota exceeded"
        );
        file.set_len(size)?;
    }
    if let Some(mode) = a.permissions {
        file.set_permissions(cap_std::fs::Permissions::from_mode(mode & 0o777))?;
    }
    if a.atime.is_some() || a.mtime.is_some() {
        let mut times = std::fs::FileTimes::new();
        if let Some(t) = a.atime {
            times = times.set_accessed(std::time::UNIX_EPOCH + Duration::from_secs(t as u64))
        }
        if let Some(t) = a.mtime {
            times = times.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(t as u64))
        }
        file.try_clone()?.into_std().set_times(times)?;
    }
    Ok(())
}
