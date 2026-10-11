use crate::{config::Config, engine::Engine};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn limit(config: &Config, settings: &Value) -> Result<u64> {
    let configured = settings["build"]["disk_space"].as_u64().unwrap_or(0);
    let mb = if configured == 0 {
        config.system.default_disk_limit_mb
    } else {
        configured
    };
    mb.checked_mul(1024 * 1024)
        .filter(|n| *n > 0)
        .context("Invalid server disk quota")
}

fn command(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().context("Storage path must be UTF-8")
}

#[cfg(target_os = "macos")]
fn image_size(image: &Path) -> Result<u64> {
    let info = plist::Value::from_reader(std::io::Cursor::new(command(
        "/usr/bin/hdiutil",
        &["imageinfo", "-plist", text(image)?],
    )?))?;
    info.as_dictionary()
        .and_then(|v| v.get("Size Information"))
        .and_then(plist::Value::as_dictionary)
        .and_then(|v| v.get("Total Bytes"))
        .and_then(plist::Value::as_unsigned_integer)
        .context("Missing disk image capacity")
}

#[cfg(target_os = "macos")]
fn mounted_image(directory: &Path) -> Result<Option<PathBuf>> {
    let directory = directory.canonicalize()?;
    let info = plist::Value::from_reader(std::io::Cursor::new(command(
        "/usr/bin/hdiutil",
        &["info", "-plist"],
    )?))?;
    let images = info
        .as_dictionary()
        .and_then(|v| v.get("images"))
        .and_then(plist::Value::as_array)
        .context("Missing disk image list")?;
    for image in images {
        let Some(image) = image.as_dictionary() else {
            continue;
        };
        let mounted = image
            .get("system-entities")
            .and_then(plist::Value::as_array)
            .is_some_and(|entities| {
                entities.iter().any(|entity| {
                    entity
                        .as_dictionary()
                        .and_then(|v| v.get("mount-point"))
                        .and_then(plist::Value::as_string)
                        .is_some_and(|p| Path::new(p) == directory)
                })
            });
        if mounted {
            return image
                .get("image-path")
                .and_then(plist::Value::as_string)
                .map(PathBuf::from)
                .map(Some)
                .context("Missing mounted disk image path");
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn create_image(image: &Path, bytes: u64, id: &str) -> Result<()> {
    command(
        "/usr/bin/hdiutil",
        &[
            "create",
            "-size",
            &format!("{}m", bytes / (1024 * 1024)),
            "-type",
            "SPARSE",
            "-fs",
            "HFS+",
            "-volname",
            &format!("Panasaurus-{id}"),
            text(image)?,
        ],
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn attach_image(image: &Path, directory: &Path) -> Result<()> {
    command(
        "/usr/bin/hdiutil",
        &[
            "attach",
            "-nobrowse",
            "-owners",
            "on",
            "-mountpoint",
            text(directory)?,
            text(image)?,
        ],
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn image_size(image: &Path) -> Result<u64> {
    Ok(image.metadata()?.len())
}

#[cfg(target_os = "linux")]
fn mounted_image(directory: &Path) -> Result<Option<PathBuf>> {
    let output = command(
        "findmnt",
        &[
            "--json",
            "--target",
            text(directory)?,
            "--output",
            "SOURCE,TARGET",
        ],
    )?;
    let info: Value = serde_json::from_slice(&output)?;
    let mount = &info["filesystems"][0];
    if mount["target"].as_str() != directory.to_str() {
        return Ok(None);
    }
    let source = mount["source"].as_str().context("Missing mount source")?;
    let backing = command(
        "losetup",
        &["--noheadings", "--raw", "--output", "BACK-FILE", source],
    )?;
    Ok(Some(PathBuf::from(String::from_utf8(backing)?.trim())))
}

#[cfg(target_os = "linux")]
fn create_image(image: &Path, bytes: u64, _: &str) -> Result<()> {
    ensure!(
        bytes >= 16 * 1024 * 1024,
        "Linux disk quotas must be at least 16 MiB"
    );
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(image)?;
    file.set_len(bytes)?;
    command("mkfs.ext4", &["-F", "-m", "0", text(image)?])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn attach_image(image: &Path, directory: &Path) -> Result<()> {
    command(
        "mount",
        &["-o", "loop,nosuid,nodev", text(image)?, text(directory)?],
    )?;
    Ok(())
}

fn copy_data(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let metadata = entry.path().symlink_metadata()?;
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            std::fs::create_dir(&target)?;
            copy_data(&entry.path(), &target)?;
            std::fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else if metadata.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, target)?;
        } else {
            anyhow::bail!("Remove special files before migrating server storage");
        }
    }
    Ok(())
}

fn prepare(config: &Config, settings: &Value) -> Result<()> {
    let id = settings["uuid"].as_str().context("Missing server UUID")?;
    Engine::name(id)?;
    let directory = config.system.data.join(id);
    let bytes = limit(config, settings)?;
    let image_root = config.system.data.canonicalize()?.join(".quota-images");
    std::fs::create_dir_all(&image_root)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&image_root, std::fs::Permissions::from_mode(0o700))?;
    let extension = if cfg!(target_os = "macos") {
        "sparseimage"
    } else {
        "ext4"
    };
    let image = image_root.join(format!("{id}.{extension}"));
    std::fs::create_dir_all(&directory)?;
    ensure!(
        !directory.symlink_metadata()?.is_symlink(),
        "Server storage cannot be a symlink"
    );
    let marker = image_root.join(format!("{id}.ready"));
    if let Some(mounted) = mounted_image(&directory)? {
        ensure!(
            marker.is_file(),
            "Storage migration is incomplete; restore the original backup before retrying"
        );
        ensure!(
            mounted.canonicalize()? == image.canonicalize()?,
            "Server has an unexpected storage device"
        );
        ensure!(
            image_size(&image)? == bytes,
            "Resize the server disk image offline before changing its quota"
        );
        ensure!(
            directory.join(".server-data").is_dir(),
            "Migrate the storage layout offline before starting the server"
        );
        return Ok(());
    }
    if !image.exists() {
        create_image(&image, bytes, id)?;
    }
    ensure!(
        image_size(&image)? == bytes,
        "Server disk image capacity does not match its quota"
    );
    let has_data = std::fs::read_dir(&directory)?.next().is_some();
    let backup = image_root.join(format!("{id}.backup"));
    if has_data {
        ensure!(
            !backup.exists(),
            "A storage migration backup already exists; resolve it before retrying"
        );
        std::fs::rename(&directory, &backup)?;
        std::fs::create_dir(&directory)?;
    }
    if let Err(error) = attach_image(&image, &directory) {
        if has_data {
            std::fs::remove_dir(&directory)?;
            std::fs::rename(&backup, &directory)?;
        }
        return Err(error);
    }
    let payload = directory.join(".server-data");
    std::fs::create_dir_all(&payload)?;
    if has_data {
        copy_data(&backup, &payload).context("Storage migration failed; original files remain in .quota-images/<uuid>.backup. Do not start until migration is complete.")?;
    }
    #[cfg(target_os = "linux")]
    command("chown", &["-hR", &config.docker.user, text(&payload)?])?;
    ensure!(
        mounted_image(&directory)?.as_ref() == Some(&image),
        "Storage mount verification failed"
    );
    std::fs::write(marker, bytes.to_string())?;
    Ok(())
}

pub async fn provision(config: &Config, settings: &Value) -> Result<()> {
    let config = config.clone();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || prepare(&config, &settings)).await?
}

pub async fn verify(config: &Config, settings: &Value) -> Result<()> {
    let config = config.clone();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || {
        let id = settings["uuid"].as_str().context("Missing server UUID")?;
        let directory = config.system.data.join(id);
        let extension = if cfg!(target_os = "macos") {
            "sparseimage"
        } else {
            "ext4"
        };
        let image = config
            .system
            .data
            .canonicalize()?
            .join(".quota-images")
            .join(format!("{id}.{extension}"));
        ensure!(
            config
                .system
                .data
                .join(".quota-images")
                .join(format!("{id}.ready"))
                .is_file(),
            "Storage migration is incomplete"
        );
        ensure!(
            mounted_image(&directory)?.as_ref() == Some(&image),
            "Server storage quota is not mounted. Stop the server and provision storage first."
        );
        ensure!(
            directory.join(".server-data").is_dir(),
            "Server file storage is missing"
        );
        ensure!(
            image_size(&image)? == limit(&config, &settings)?,
            "Server storage quota has changed; resize it offline first"
        );
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Mounts a temporary quota image; requires macOS disk images or Linux root"]
    async fn mounted_storage_is_bounded_and_failed_migrations_block_startup() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let config: Config = serde_json::from_value(serde_json::json!({
            "uuid":uuid::Uuid::new_v4().to_string(),"token_id":"fixture","token":"a".repeat(64),
            "remote":"http://127.0.0.1:8080","jwt_audience":"http://127.0.0.1:8081",
            "api":{"host":"127.0.0.1","port":8081},"system":{"data":root.path()},
            "docker":{"user":"0:0"}
        }))
        .unwrap();
        let settings = serde_json::json!({"uuid":id,"build":{"disk_space":32}});
        let directory = root.path().join(&id);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("existing.txt"), "retained data").unwrap();
        provision(&config, &settings).await.unwrap();
        verify(&config, &settings).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join(".server-data/existing.txt")).unwrap(),
            "retained data"
        );
        assert_eq!(
            std::fs::read_to_string(
                root.path()
                    .join(".quota-images")
                    .join(format!("{id}.backup/existing.txt"))
            )
            .unwrap(),
            "retained data"
        );
        let mut changed = settings.clone();
        changed["build"]["disk_space"] = serde_json::json!(64);
        assert!(verify(&config, &changed).await.is_err());
        std::fs::remove_file(
            root.path()
                .join(".quota-images")
                .join(format!("{id}.ready")),
        )
        .unwrap();
        assert!(provision(&config, &settings).await.is_err());
        assert!(verify(&config, &settings).await.is_err());
        #[cfg(target_os = "macos")]
        command("/usr/bin/hdiutil", &["detach", text(&directory).unwrap()]).unwrap();
        #[cfg(target_os = "linux")]
        command("umount", &[text(&directory).unwrap()]).unwrap();
    }
}
