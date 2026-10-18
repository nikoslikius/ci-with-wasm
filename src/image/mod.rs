use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use nix::fcntl::{Flock, FlockArg};
use oci_unpack::{Reference, Unpacker};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

const IMAGE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Handle the private child-process protocol used to isolate oci-unpack's
/// Landlock restrictions from the later container setup.
pub(crate) fn handle_internal_unpack() -> anyhow::Result<bool> {
    let raw_args: Vec<_> = std::env::args_os().collect();
    if !raw_args
        .get(1)
        .is_some_and(|arg| arg == OsStr::new("__internal_unpack"))
    {
        return Ok(false);
    }

    let image = raw_args
        .get(2)
        .context("missing image for internal unpack")?;
    let bundle = raw_args
        .get(3)
        .context("missing bundle path for internal unpack")?;
    unpack_image(image.to_string_lossy().as_ref(), PathBuf::from(bundle))?;
    Ok(true)
}

pub(crate) fn pull_and_unpack(image: &str, bundle: &std::path::Path) -> anyhow::Result<()> {
    let cache_root = crate::storage::image_cache_root()?;
    let cache_key = image_cache_key(image);
    let cache_dir = cache_root.join(&cache_key);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache_root.join(format!("{cache_key}.lock")))
        .context("open image cache lock")?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, error)| anyhow::anyhow!("lock image cache entry: {error}"))?;

    if image_cache_is_fresh(&cache_dir, image) {
        info!(image, "using cached image");
    } else {
        info!(image, "pulling and unpacking image");
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir).context("remove expired image cache entry")?;
        }
        let temp_bundle = cache_root.join(format!(".{cache_key}-{}", unique_suffix()?));
        fs::create_dir(&temp_bundle).context("create temporary image cache entry")?;
        if let Err(error) = unpack_in_helper(image, &temp_bundle) {
            let _ = fs::remove_dir_all(&temp_bundle);
            return Err(error);
        }
        fs::write(temp_bundle.join("image-reference"), image)
            .context("write image cache metadata")?;
        if let Err(error) = fs::rename(&temp_bundle, &cache_dir) {
            let _ = fs::remove_dir_all(&temp_bundle);
            return Err(error).context("publish image cache entry");
        }
    }

    let cached_rootfs = cache_dir.join("rootfs");
    let run_rootfs = bundle.join("rootfs");
    copy_rootfs(&cached_rootfs, &run_rootfs)
        .with_context(|| format!("copy cached image rootfs for {image}"))?;
    Ok(())
}

fn unpack_in_helper(image: &str, bundle: &Path) -> anyhow::Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg("__internal_unpack")
        .arg(image)
        // `oci-unpack` takes a bundle path and creates bundle/rootfs itself.
        .arg(bundle)
        .status()
        .context("start image unpack helper")?;
    if !status.success() {
        bail!("image unpack helper exited with {status}");
    }
    Ok(())
}

fn image_cache_key(image: &str) -> String {
    let digest = Sha256::digest(image.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unique_suffix() -> anyhow::Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    Ok(format!("{}-{timestamp}", std::process::id()))
}

fn image_cache_is_fresh(cache_dir: &Path, image: &str) -> bool {
    let rootfs = cache_dir.join("rootfs");
    let marker = cache_dir.join("image-reference");
    if !rootfs.is_dir() || fs::read_to_string(&marker).ok().as_deref() != Some(image) {
        return false;
    }
    marker
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < IMAGE_CACHE_TTL)
}

fn copy_rootfs(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir(destination).context("create per-run rootfs")?;
    let mut hard_links = HashMap::new();
    copy_directory_contents(source, destination, &mut hard_links)?;
    copy_metadata(source, destination)?;
    Ok(())
}

fn copy_directory_contents(
    source: &Path,
    destination: &Path,
    hard_links: &mut HashMap<(u64, u64), PathBuf>,
) -> anyhow::Result<()> {
    for item in fs::read_dir(source)
        .with_context(|| format!("read image directory {}", source.display()))?
    {
        let item = item.context("read image directory entry")?;
        let source_path = item.path();
        let destination_path = destination.join(item.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .with_context(|| format!("inspect image path {}", source_path.display()))?;
        let kind = metadata.file_type();

        if kind.is_symlink() {
            let target = fs::read_link(&source_path)
                .with_context(|| format!("read image symlink {}", source_path.display()))?;
            std::os::unix::fs::symlink(target, &destination_path)
                .with_context(|| format!("copy image symlink {}", source_path.display()))?;
            copy_xattrs(&source_path, &destination_path)?;
        } else if kind.is_dir() {
            fs::create_dir(&destination_path).with_context(|| {
                format!("create image directory {}", destination_path.display())
            })?;
            copy_directory_contents(&source_path, &destination_path, hard_links)?;
            copy_metadata(&source_path, &destination_path)?;
        } else if kind.is_file() {
            let inode = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1
                && let Some(first_copy) = hard_links.get(&inode)
            {
                fs::hard_link(first_copy, &destination_path)
                    .with_context(|| format!("copy image hard link {}", source_path.display()))?;
            } else {
                fs::copy(&source_path, &destination_path)
                    .with_context(|| format!("copy image file {}", source_path.display()))?;
                let mut writable_permissions = metadata.permissions();
                writable_permissions.set_mode(writable_permissions.mode() | 0o200);
                fs::set_permissions(&destination_path, writable_permissions)
                    .with_context(|| format!("prepare image file {}", source_path.display()))?;
                copy_xattrs(&source_path, &destination_path)?;
                fs::set_permissions(&destination_path, metadata.permissions())
                    .with_context(|| format!("copy image permissions {}", source_path.display()))?;
                if metadata.nlink() > 1 {
                    hard_links.insert(inode, destination_path);
                }
            }
        } else {
            bail!(
                "image contains unsupported special file {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn copy_metadata(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect image metadata {}", source.display()))?;
    copy_xattrs(source, destination)?;
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("copy image permissions {}", source.display()))?;
    Ok(())
}

fn copy_xattrs(source: &Path, destination: &Path) -> anyhow::Result<()> {
    for name in xattr::list(source)
        .with_context(|| format!("list image attributes {}", source.display()))?
    {
        let Some(value) = xattr::get(source, &name).with_context(|| {
            format!(
                "read image attribute {} on {}",
                name.to_string_lossy(),
                source.display()
            )
        })?
        else {
            continue;
        };
        if let Err(error) = xattr::set(destination, &name, &value) {
            if name == "security.capability" {
                debug!(path = %destination.display(), "skipping file capabilities when copying rootless image cache");
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "copy image attribute {} to {}",
                        name.to_string_lossy(),
                        destination.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn unpack_image(image: &str, bundle: PathBuf) -> anyhow::Result<()> {
    let reference = Reference::try_from(image).context("invalid image reference")?;
    Unpacker::new(reference)
        .unpack(bundle)
        .context("pull and unpack image")
}
