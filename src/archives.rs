use crate::sandbox::{Sandbox, relative};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    collections::HashSet,
    io::{Read, Seek},
    path::{Path, PathBuf},
};

pub fn pack(
    sandbox: &Sandbox,
    root: &str,
    selected: &[String],
    destination: &Path,
    ignore: &str,
) -> Result<()> {
    ensure!(
        selected.len() <= 1000 && ignore.len() <= 64 * 1024,
        "Archive selection too large"
    );
    let base = sandbox.checked(root, true)?;
    let mut patterns = ignore::gitignore::GitignoreBuilder::new("/");
    for line in ignore.lines() {
        patterns.add_line(None, line)?;
    }
    let patterns = patterns.build()?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut tar = tar::Builder::new(GzEncoder::new(f, Compression::fast()));
    let mut seen = HashSet::new();
    let mut count = 0;
    let mut bytes = 0u64;
    let paths = if selected.is_empty() {
        vec![base.clone()]
    } else {
        selected
            .iter()
            .map(|v| sandbox.checked(v, false).map(|p| base.join(p)))
            .collect::<Result<Vec<_>>>()?
    };
    for path in paths {
        sandbox.walk(&path, &mut count, 0, &mut |path, meta, file| {
            let relative = path.strip_prefix(&base)?;
            if relative.as_os_str().is_empty()
                || patterns
                    .matched_path_or_any_parents(relative, meta.is_dir())
                    .is_ignore()
            {
                return Ok(());
            }
            ensure!(
                seen.insert(relative.to_owned()),
                "Overlapping archive selection"
            );
            let mut h = tar::Header::new_gnu();
            h.set_mode(meta.permissions().mode() & 0o777);
            h.set_mtime(0);
            h.set_uid(0);
            h.set_gid(0);
            if meta.is_dir() {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                tar.append_data(&mut h, relative, std::io::empty())?;
            } else {
                bytes = bytes
                    .checked_add(meta.len())
                    .context("Archive size overflow")?;
                ensure!(bytes <= sandbox.quota, "Archive exceeds server quota");
                h.set_size(meta.len());
                h.set_entry_type(tar::EntryType::Regular);
                tar.append_data(
                    &mut h,
                    relative,
                    file.context("Missing file")?.take(meta.len()),
                )?;
            }
            Ok(())
        })?;
    }
    let gz = tar.into_inner()?;
    gz.finish()?.sync_all()?;
    Ok(())
}
use cap_std::fs::PermissionsExt;
#[derive(Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub directory: bool,
    pub size: u64,
    pub mode: u32,
}
pub struct Staged {
    pub directory: tempfile::TempDir,
    pub entries: Vec<Entry>,
    pub bytes: u64,
}
pub fn stage(source: &Path, private_root: &Path, limit: u64) -> Result<Staged> {
    crate::sandbox::private_directory(private_root)?;
    let directory = tempfile::Builder::new()
        .prefix("archive-")
        .tempdir_in(private_root)?;
    let mut staged = Staged {
        directory,
        entries: Vec::new(),
        bytes: 0,
    };
    let mut seen = HashSet::new();
    let mut input = std::fs::File::open(source)?;
    let mut magic = [0u8; 4];
    let n = input.read(&mut magic)?;
    input.rewind()?;
    if n == 4 && magic.starts_with(b"PK") {
        let mut zip = zip::ZipArchive::new(input)?;
        ensure!(zip.len() <= 100_000, "Too many archive entries");
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let name = entry.name()?;
            ensure!(!name.starts_with('/'), "Absolute archive path rejected");
            let p = relative(&name, false)?;
            let mode = entry.unix_mode().unwrap_or(0o644);
            ensure!(
                mode & 0o170000 == 0 || mode & 0o170000 == 0o100000 || mode & 0o170000 == 0o040000,
                "Archive links and special files are forbidden"
            );
            let e = Entry {
                path: p,
                directory: entry.is_dir(),
                size: entry.size(),
                mode: mode & 0o777,
            };
            unpack(&mut staged, &mut seen, e, &mut entry, limit)?;
        }
    } else {
        let reader: Box<dyn Read> = if magic[..n].starts_with(&[0x1f, 0x8b]) {
            Box::new(GzDecoder::new(input))
        } else {
            Box::new(input)
        };
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() || kind.is_dir(),
                "Archive links, sparse and special files are forbidden"
            );
            let raw = entry
                .path()?
                .to_str()
                .context("Non-UTF8 archive name")?
                .to_owned();
            if kind.is_dir() && (raw == "." || raw == "./") {
                continue;
            }
            ensure!(!raw.starts_with('/'), "Absolute archive path rejected");
            let e = Entry {
                path: relative(&raw, false)?,
                directory: kind.is_dir(),
                size: entry.size(),
                mode: entry.header().mode()? & 0o777,
            };
            unpack(&mut staged, &mut seen, e, &mut entry, limit)?;
        }
    }
    Ok(staged)
}
fn unpack(
    staged: &mut Staged,
    seen: &mut HashSet<String>,
    entry: Entry,
    reader: &mut impl Read,
    limit: u64,
) -> Result<()> {
    ensure!(
        staged.entries.len() < 100_000 && entry.path.components().count() <= 128,
        "Archive traversal limit exceeded"
    );
    ensure!(
        seen.insert(entry.path.to_string_lossy().to_lowercase()),
        "Duplicate archive path"
    );
    let output = staged.directory.path().join(&entry.path);
    if entry.directory {
        std::fs::create_dir_all(&output)?;
    } else {
        staged.bytes = staged
            .bytes
            .checked_add(entry.size)
            .context("Archive size overflow")?;
        ensure!(staged.bytes <= limit, "Expanded archive exceeds quota");
        std::fs::create_dir_all(output.parent().context("Missing parent")?)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?;
        ensure!(
            std::io::copy(&mut reader.take(entry.size.saturating_add(1)), &mut f)? == entry.size,
            "Invalid archive size"
        );
        f.sync_all()?;
    }
    staged.entries.push(entry);
    Ok(())
}
pub fn apply(staged: &Staged, sandbox: &Sandbox, root: &str, exclusive: bool) -> Result<()> {
    let base = sandbox.checked(root, true)?;
    let mut growth = 0u64;

    for entry in &staged.entries {
        let p = base.join(&entry.path);
        sandbox.checked(p.to_str().context("Invalid destination")?, false)?;
        let _ = sandbox.parent(&p, false).or_else(|_| {
            let mut prefix = PathBuf::new();
            for part in p.parent().into_iter().flat_map(Path::components) {
                prefix.push(part.as_os_str());
                match sandbox.root.symlink_metadata(&prefix) {
                    Ok(m) => ensure!(m.is_dir() && !m.is_symlink(), "Unsafe extraction parent"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                    Err(e) => return Err(e.into()),
                }
            }
            Ok((sandbox.root.try_clone()?, p.clone()))
        })?;
        use cap_std::fs::MetadataExt;
        match sandbox.root.symlink_metadata(&p) {
            Ok(m) => {
                ensure!(!m.is_symlink(), "Extraction alias rejected");
                if entry.directory {
                    ensure!(m.is_dir(), "Directory conflicts with file");
                } else {
                    ensure!(
                        !exclusive && m.is_file() && m.nlink() == 1,
                        "Extraction would overwrite an existing or aliased file"
                    );
                    growth = growth
                        .checked_add(entry.size.saturating_sub(m.len()))
                        .context("Quota overflow")?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                growth = growth
                    .checked_add(if entry.directory { 0 } else { entry.size })
                    .context("Quota overflow")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        sandbox
            .usage()?
            .checked_add(growth)
            .is_some_and(|n| n <= sandbox.quota),
        "Expanded archive exceeds remaining quota"
    );
    for entry in &staged.entries {
        let target = base.join(&entry.path);
        let (dir, name) = sandbox.parent(&target, true)?;
        if entry.directory {
            match dir.create_dir(&name) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    dir.open_dir_nofollow(name)?;
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            let mut f = std::fs::File::open(staged.directory.path().join(&entry.path))?;
            sandbox.write(
                target.to_str().context("Invalid path")?,
                &mut f,
                sandbox.quota,
                exclusive,
            )?;
            sandbox.chmod(target.to_str().unwrap(), entry.mode)?;
        }
    }
    Ok(())
}
use cap_fs_ext::DirExt;
