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
        mounted_image(&directory)?.is_some_and(|p| p.canonicalize().is_ok_and(|p| p == image)),
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
            mounted_image(&directory)?.is_some_and(|p| p.canonicalize().is_ok_and(|p| p == image)),
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

pub async fn resize(config: &Config, settings: &Value) -> Result<()> {
    let config = config.clone();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let id = settings["uuid"].as_str().context("Missing UUID")?;
        Engine::name(id)?;
        let base = config.system.data.canonicalize()?;
        let images = base.join(".quota-images");
        ensure!(
            !images.symlink_metadata()?.is_symlink(),
            "Quota directory alias rejected"
        );
        let extension = if cfg!(target_os = "macos") {
            "sparseimage"
        } else {
            "ext4"
        };
        let image = images.join(format!("{id}.{extension}"));
        if !image.exists() {
            return Ok(());
        }
        ensure!(
            image.symlink_metadata()?.is_file() && !image.symlink_metadata()?.is_symlink(),
            "Quota image alias rejected"
        );
        let desired = limit(&config, &settings)?;
        if image_size(&image)? == desired {
            return Ok(());
        }
        let mount = base.join(id);
        let mounted = mounted_image(&mount)?;
        if let Some(source) = &mounted {
            ensure!(
                source.canonicalize()? == image.canonicalize()?,
                "Unexpected quota mount"
            );
            #[cfg(target_os = "macos")]
            command("/usr/bin/hdiutil", &["detach", text(&mount)?])?;
            #[cfg(target_os = "linux")]
            command("umount", &[text(&mount)?])?;
        }
        let resized = (|| -> Result<()> {
            #[cfg(target_os = "macos")]
            {
                let limits = String::from_utf8(command(
                    "/usr/bin/hdiutil",
                    &["resize", "-alllimits", text(&image)?],
                )?)?;
                let rows: Vec<Vec<&str>> = limits
                    .lines()
                    .map(|line| line.split_whitespace().collect())
                    .collect();
                let image_row = rows
                    .iter()
                    .find(|r| r.first() == Some(&"image"))
                    .context("Image limits missing")?;
                let partitions: Vec<_> = rows
                    .iter()
                    .filter(|r| r.last() == Some(&"Apple_HFS"))
                    .collect();
                ensure!(
                    partitions.len() == 1,
                    "Quota image must have exactly one HFS partition"
                );
                let total = image_row
                    .get(2)
                    .context("Invalid image limits")?
                    .parse::<u64>()?;
                let partition = partitions[0]
                    .get(2)
                    .context("Invalid partition limits")?
                    .parse::<u64>()?;
                let overhead = total
                    .checked_sub(partition)
                    .and_then(|n| n.checked_mul(512))
                    .context("Invalid partition overhead")?;
                let payload = desired
                    .checked_sub(overhead)
                    .context("Quota below partition overhead")?;
                command(
                    "/usr/bin/hdiutil",
                    &["resize", "-size", &payload.to_string(), text(&image)?],
                )?;
            }
            #[cfg(target_os = "linux")]
            {
                let old = image_size(&image)?;
                let checked = Command::new("e2fsck")
                    .args(["-f", "-p", text(&image)?])
                    .output()?;
                ensure!(
                    matches!(checked.status.code(), Some(0 | 1)),
                    "Filesystem check failed"
                );
                let file = std::fs::OpenOptions::new().write(true).open(&image)?;
                if desired > old {
                    file.set_len(desired)?;
                }
                command(
                    "resize2fs",
                    &[text(&image)?, &format!("{}M", desired / (1024 * 1024))],
                )?;
                if desired < old {
                    file.set_len(desired)?;
                }
                file.sync_all()?;
            }
            Ok(())
        })();
        if mounted.is_some() {
            attach_image(&image, &mount)?;
        }
        resized?;
        ensure!(
            image_size(&image)? == desired,
            "Quota resize verification failed"
        );
        std::fs::write(images.join(format!("{id}.ready")), desired.to_string())?;
        Ok(())
    })
    .await?
}

pub async fn destroy(config: &Config, id: &str) -> Result<()> {
    Engine::name(id)?;
    let config = config.clone();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use cap_std::{ambient_authority, fs::Dir};
        let base = config.system.data.canonicalize()?;
        let mount = base.join(&id);
        let images = base.join(".quota-images");
        let extension = if cfg!(target_os = "macos") {
            "sparseimage"
        } else {
            "ext4"
        };
        let image = images.join(format!("{id}.{extension}"));
        if mount.exists() {
            ensure!(
                !mount.symlink_metadata()?.is_symlink(),
                "Server storage alias rejected"
            );
            if let Some(source) = mounted_image(&mount)? {
                ensure!(
                    source.canonicalize()? == image.canonicalize()?,
                    "Unexpected quota mount"
                );
                #[cfg(target_os = "macos")]
                command("/usr/bin/hdiutil", &["detach", text(&mount)?])?;
                #[cfg(target_os = "linux")]
                command("umount", &[text(&mount)?])?;
            }
            Dir::open_ambient_dir(&base, ambient_authority())?.remove_dir_all(&id)?;
        }
        if images.exists() {
            ensure!(
                !images.symlink_metadata()?.is_symlink(),
                "Quota directory alias rejected"
            );
            let dir = Dir::open_ambient_dir(&images, ambient_authority())?;
            for suffix in [extension, "ready"] {
                let name = format!("{id}.{suffix}");
                match dir.remove_file(name) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let backup = format!("{id}.backup");
            if dir.try_exists(&backup)? {
                dir.remove_dir_all(backup)?;
            }
        }
        Ok(())
    })
    .await?
}
