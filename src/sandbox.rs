use anyhow::{Context, Result, ensure};
use cap_fs_ext::DirExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, File, MetadataExt, OpenOptions, OpenOptionsExt, Permissions, PermissionsExt},
};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

pub struct Sandbox {
    pub root: Dir,
    denied: Gitignore,
    pub quota: u64,
}
impl Sandbox {
    pub fn new(root: &Path, settings: &Value, config: &crate::config::Config) -> Result<Self> {
        let mut patterns = GitignoreBuilder::new(root);
        if let Some(list) = settings["egg"]["file_denylist"].as_array() {
            for pattern in list {
                patterns.add_line(None, pattern.as_str().context("Invalid denylist")?)?;
            }
        }
        Ok(Self {
            root: Dir::open_ambient_dir(root, ambient_authority())?,
            denied: patterns.build()?,
            quota: crate::storage::limit(config, settings)?,
        })
    }
    pub fn checked(&self, value: &str, allow_root: bool) -> Result<PathBuf> {
        let p = relative(value, allow_root)?;
        ensure!(
            !self
                .denied
                .matched_path_or_any_parents(&p, true)
                .is_ignore(),
            "File is protected by the egg"
        );
        Ok(p)
    }
    pub fn parent(&self, path: &Path, create: bool) -> Result<(Dir, PathBuf)> {
        let name = path.file_name().context("A file name is required")?;
        let mut dir = self.root.try_clone()?;
        if let Some(parent) = path.parent() {
            for part in parent.components() {
                let Component::Normal(part) = part else {
                    anyhow::bail!("Invalid file path")
                };
                if create {
                    match dir.create_dir(part) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                dir = dir.open_dir_nofollow(part)?;
            }
        }
        Ok((dir, PathBuf::from(name)))
    }
    pub fn open(
        &self,
        value: &str,
        write: bool,
        create: bool,
        exclusive: bool,
        truncate: bool,
    ) -> Result<File> {
        let p = self.checked(value, false)?;
        let (dir, name) = self.parent(&p, false)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(write)
            .create(create)
            .create_new(exclusive)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o644);
        let file = dir.open_with(&name, &options)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "Only regular, unaliased files are supported"
        );
        if truncate {
            ensure!(write, "Truncation requires write access");
            file.set_len(0)?;
        }
        Ok(file)
    }
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            root: self.root.try_clone()?,
            denied: self.denied.clone(),
            quota: self.quota,
        })
    }
    pub fn usage(&self) -> Result<u64> {
        Ok(crate::files::disk_usage(&self.root)?)
    }
    pub fn write(
        &self,
        value: &str,
        reader: &mut impl Read,
        max: u64,
        exclusive: bool,
    ) -> Result<u64> {
        let p = self.checked(value, false)?;
        let (dir, name) = self.parent(&p, false)?;
        let old = match dir.symlink_metadata(&name) {
            Ok(m) => {
                ensure!(m.is_file() && m.nlink() == 1, "Unsafe destination");
                ensure!(!exclusive, "Destination exists");
                m.len()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        let available = self
            .quota
            .checked_sub(self.usage()?.saturating_sub(old))
            .context("Disk quota exceeded")?
            .min(max);
        let temporary = format!(".panasaurus-write-{}", uuid::Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut output = dir.open_with(&temporary, &options)?;
        let result = (|| -> Result<u64> {
            let count = std::io::copy(&mut reader.take(available.saturating_add(1)), &mut output)?;
            ensure!(count <= available, "File exceeds quota or transfer limit");
            output.sync_all()?;
            output.set_permissions(Permissions::from_mode(0o644))?;
            if exclusive {
                dir.hard_link(&temporary, &dir, &name)?;
                dir.remove_file(&temporary)?;
            } else {
                dir.rename(&temporary, &dir, &name)?;
            }
            Ok(count)
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temporary);
        }
        result
    }
    pub fn write_at(&self, file: &mut File, offset: u64, data: &[u8]) -> Result<()> {
        let m = file.metadata()?;
        ensure!(m.is_file() && m.nlink() == 1, "File alias rejected");
        let new = offset
            .checked_add(data.len() as u64)
            .context("Offset overflow")?;
        ensure!(new <= self.quota, "File exceeds quota");
        let growth = new.saturating_sub(m.len());
        ensure!(
            self.usage()?
                .checked_add(growth)
                .is_some_and(|n| n <= self.quota),
            "Disk quota exceeded"
        );
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        Ok(())
    }
    pub fn mkdir(&self, value: &str) -> Result<()> {
        let p = self.checked(value, false)?;
        let (dir, name) = self.parent(&p, false)?;
        dir.create_dir(&name)?;
        Ok(())
    }
    pub fn chmod(&self, value: &str, mode: u32) -> Result<()> {
        ensure!(mode & !0o777 == 0, "Special permission bits are forbidden");
        let p = self.checked(value, false)?;
        let (d, n) = self.parent(&p, false)?;
        let m = d.symlink_metadata(&n)?;
        if m.is_dir() {
            d.open_dir_nofollow(n)?
                .set_permissions(".", Permissions::from_mode(mode))?;
        } else {
            self.open(value, false, false, false, false)?
                .set_permissions(Permissions::from_mode(mode))?;
        }
        Ok(())
    }
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let p = self.checked(from, false)?;
        let q = self.checked(to, false)?;
        ensure!(!q.starts_with(&p), "Cannot move a directory into itself");
        self.walk(&p, &mut 0, 0, &mut |entry, _, _| {
            let target = q.join(entry.strip_prefix(&p)?);
            self.checked(target.to_str().context("Invalid path")?, false)?;
            Ok(())
        })?;
        let (a, n) = self.parent(&p, false)?;
        let (b, m) = self.parent(&q, false)?;
        ensure!(
            b.symlink_metadata(&m)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Destination exists"
        );
        a.rename(n, &b, m)?;
        Ok(())
    }
    pub fn validate_tree(&self, path: &Path) -> Result<()> {
        let mut count = 0;
        self.walk(path, &mut count, 0, &mut |_, _, _| Ok(()))
    }
    pub fn walk(
        &self,
        path: &Path,
        count: &mut usize,
        depth: usize,
        callback: &mut impl FnMut(&Path, &cap_std::fs::Metadata, Option<File>) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            depth <= 128 && *count < 100_000,
            "Filesystem traversal limit exceeded"
        );
        *count += 1;
        self.checked(
            path.to_str().context("Non-UTF8 path")?,
            path.as_os_str().is_empty(),
        )?;
        let (dir, name) = if path.as_os_str().is_empty() {
            (self.root.try_clone()?, PathBuf::from("."))
        } else {
            self.parent(path, false)?
        };
        let meta = dir.symlink_metadata(&name)?;
        ensure!(!meta.is_symlink(), "Symlink aliases are forbidden");
        if meta.is_dir() {
            callback(path, &meta, None)?;
            let sub = dir.open_dir_nofollow(&name)?;
            for entry in sub.entries()? {
                let entry = entry?;
                self.walk(&path.join(entry.file_name()), count, depth + 1, callback)?;
            }
        } else {
            ensure!(
                meta.is_file() && meta.nlink() == 1,
                "File aliases or special files are forbidden"
            );
            let file = self.open(
                path.to_str().context("Non-UTF8 path")?,
                false,
                false,
                false,
                false,
            )?;
            callback(path, &meta, Some(file))?;
        }
        Ok(())
    }
    pub fn copy(&self, source: &str, destination: &str) -> Result<()> {
        let p = self.checked(source, false)?;
        let q = self.checked(destination, false)?;
        ensure!(!q.starts_with(&p), "Cannot copy into the source");
        self.validate_tree(&p)?;
        self.walk(&p, &mut 0, 0, &mut |entry, meta, file| {
            let target = q.join(entry.strip_prefix(&p)?);
            let text = target.to_str().context("Non-UTF8 path")?;
            self.checked(text, false)?;
            let (dir, name) = self.parent(&target, false)?;
            if meta.is_dir() {
                dir.create_dir(name)?;
            } else {
                self.write(text, &mut file.context("Missing source")?, self.quota, true)?;
            }
            Ok(())
        })
    }
}
pub fn relative(value: &str, allow_root: bool) -> Result<PathBuf> {
    ensure!(
        !value.contains(['\0', '\\']) && value.len() <= 4096,
        "Invalid file path"
    );
    let mut clean = PathBuf::new();
    for component in Path::new(value.trim_start_matches('/')).components() {
        match component {
            Component::Normal(p) => {
                ensure!(!p.to_string_lossy().contains(':'), "Invalid path component");
                clean.push(p)
            }
            Component::CurDir => {}
            _ => anyhow::bail!("Path escapes the server"),
        }
    }
    ensure!(
        clean.components().count() <= 128,
        "File path exceeds depth limit"
    );
    ensure!(
        allow_root || !clean.as_os_str().is_empty(),
        "A file name is required"
    );
    Ok(clean)
}
pub fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    ensure!(
        path.symlink_metadata()?.is_dir() && !path.symlink_metadata()?.is_symlink(),
        "Private directory is an alias"
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
