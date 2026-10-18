use std::{
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use cap_directories::{ProjectDirs, ambient_authority};
use cap_std::fs::Dir;

/// Holds capability-based handles for a per-run workspace. Paths are exposed
/// only at the adapter boundary because oci-unpack and libcontainer currently
/// accept host filesystem paths.
pub(crate) struct Workspace {
    _root: Dir,
    bundle: Dir,
    _state: Dir,
    path: PathBuf,
    bundle_path: PathBuf,
    state_path: PathBuf,
}

impl Workspace {
    pub(crate) fn new(work_dir: Option<&Path>) -> anyhow::Result<Self> {
        let root = match work_dir {
            Some(path) => {
                Dir::create_ambient_dir_all(path, ambient_authority())
                    .with_context(|| format!("create work directory {}", path.display()))?;
                Dir::open_ambient_dir(path, ambient_authority())
                    .with_context(|| format!("open work directory {}", path.display()))?
            }
            None => default_run_dir()?,
        };

        root.create_dir_all("bundle")
            .context("create bundle directory")?;
        root.create_dir_all("state")
            .context("create runtime state directory")?;
        let bundle = root.open_dir("bundle").context("open bundle directory")?;
        let state = root
            .open_dir("state")
            .context("open runtime state directory")?;

        let path = expose_dir_path(&root).context("resolve workspace path for runtime adapter")?;
        let bundle_path =
            expose_dir_path(&bundle).context("resolve bundle path for runtime adapter")?;
        let state_path =
            expose_dir_path(&state).context("resolve state path for runtime adapter")?;

        Ok(Self {
            _root: root,
            bundle,
            _state: state,
            path,
            bundle_path,
            state_path,
        })
    }

    pub(crate) fn bundle_dir(&self) -> &Dir {
        &self.bundle
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }
}

fn default_run_dir() -> anyhow::Result<Dir> {
    let state_root = project_state_root()?;

    state_root
        .create_dir_all("runs")
        .context("create project runs directory")?;
    let runs = state_root
        .open_dir("runs")
        .context("open project runs directory")?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let run_name = format!("run-{}-{timestamp}", std::process::id());
    runs.create_dir(&run_name)
        .with_context(|| format!("create workspace {run_name}"))?;
    runs.open_dir(&run_name)
        .with_context(|| format!("open workspace {run_name}"))
}

pub(crate) fn repository_storage_root() -> anyhow::Result<PathBuf> {
    let state_root = project_state_root()?;
    state_root
        .create_dir_all("repositories")
        .context("create project repository directory")?;
    let repositories = state_root
        .open_dir("repositories")
        .context("open project repository directory")?;
    expose_dir_path(&repositories).context("resolve repository directory for git adapter")
}

pub(crate) fn image_cache_root() -> anyhow::Result<PathBuf> {
    let project_dirs = ProjectDirs::from("org", "youki", "ci-with-wasm", ambient_authority())
        .context("locate per-user project directories")?;
    let cache_root = project_dirs
        .cache_dir()
        .context("create project cache directory")?;
    cache_root
        .create_dir_all("images")
        .context("create image cache directory")?;
    let images = cache_root
        .open_dir("images")
        .context("open image cache directory")?;
    expose_dir_path(&images).context("resolve image cache directory")
}

pub(crate) fn create_repository_id() -> anyhow::Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    Ok(format!("repo-{}-{timestamp}", std::process::id()))
}

pub(crate) fn pipeline_paths(repository_id: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    if repository_id.is_empty()
        || !repository_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid repository ID");
    }

    let state_root = project_state_root()?;
    state_root
        .create_dir_all("repositories")
        .context("create project repository directory")?;
    let repositories = state_root
        .open_dir("repositories")
        .context("open project repository directory")?;
    let artifacts_root = project_data_root()?;
    artifacts_root
        .create_dir_all("artifacts")
        .context("create project artifacts directory")?;
    let artifacts = artifacts_root
        .open_dir("artifacts")
        .context("open project artifacts directory")?;
    let repository = repositories
        .open_dir(repository_id)
        .with_context(|| format!("open managed repository {repository_id}"))?;
    artifacts
        .create_dir_all(repository_id)
        .with_context(|| format!("create artifact directory for {repository_id}"))?;
    let artifact = artifacts
        .open_dir(repository_id)
        .with_context(|| format!("open artifact directory for {repository_id}"))?;
    Ok((
        expose_dir_path(&repository).context("resolve managed repository path")?,
        expose_dir_path(&artifact).context("resolve artifact directory path")?,
    ))
}

fn project_state_root() -> anyhow::Result<Dir> {
    let project_dirs = ProjectDirs::from("org", "youki", "ci-with-wasm", ambient_authority())
        .context("locate per-user project directories")?;
    match project_dirs
        .state_dir()
        .context("create project state directory")?
    {
        Some(dir) => Ok(dir),
        None => project_dirs
            .data_local_dir()
            .context("create project local data directory"),
    }
}

fn project_data_root() -> anyhow::Result<Dir> {
    let project_dirs = ProjectDirs::from("org", "youki", "ci-with-wasm", ambient_authority())
        .context("locate per-user project directories")?;
    project_dirs
        .data_local_dir()
        .context("create project data directory")
}

/// Convert a directory capability to a stable path for the path-based runtime
/// crates. This project is Linux-only, where `/proc/self/fd` provides the
/// canonical path for an already-open directory handle.
fn expose_dir_path(dir: &Dir) -> anyhow::Result<PathBuf> {
    let fd_path = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let path = fs::canonicalize(&fd_path).with_context(|| format!("resolve {fd_path}"))?;
    if !path.is_absolute() {
        bail!(
            "directory capability resolved to non-absolute path {}",
            path.display()
        );
    }
    Ok(path)
}
