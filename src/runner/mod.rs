use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{container, image, storage::Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerRunRequest {
    image: String,
    command: Vec<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    repository_id: Option<String>,
    #[serde(default)]
    env: Vec<String>,
}

#[derive(Serialize)]
struct ContainerRunResult {
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_dir: Option<String>,
}

pub(crate) fn run_request_json(input: &str) -> anyhow::Result<String> {
    let request: ContainerRunRequest =
        serde_json::from_str(input).context("parse run_container request")?;
    let (exit_code, artifact_dir) = run_with_repository(
        &request.image,
        &request.command,
        request.id.as_deref(),
        None,
        request.repository_id.as_deref(),
        &request.env,
    )?;
    serde_json::to_string(&ContainerRunResult {
        exit_code,
        artifact_dir,
    })
    .context("serialize run result")
}

/// Handle the private subprocess protocol used to keep libcontainer's fork-based
/// startup outside the multithreaded Extism/Wasmtime host process.
pub(crate) fn handle_internal_run() -> anyhow::Result<bool> {
    let args: Vec<_> = std::env::args_os().collect();
    if !args
        .get(1)
        .is_some_and(|arg| arg == OsStr::new("__internal_run"))
    {
        return Ok(false);
    }

    let result_path = args
        .get(2)
        .map(PathBuf::from)
        .context("missing result path for internal run")?;
    let request = args
        .get(3)
        .context("missing request for internal run")?
        .to_string_lossy();
    let result = run_request_json(&request)?;
    fs::write(&result_path, result)
        .with_context(|| format!("write container result to {}", result_path.display()))?;
    Ok(true)
}

fn run_with_repository(
    image_ref: &str,
    command: &[String],
    container_id: Option<&str>,
    work_dir: Option<&Path>,
    repository_id: Option<&str>,
    additional_env: &[String],
) -> anyhow::Result<(i32, Option<String>)> {
    if image_ref.trim().is_empty() {
        bail!("image reference must not be empty");
    }
    if command.is_empty() {
        bail!("provide a command to run in the container");
    }
    for value in additional_env {
        let (key, _) = value
            .split_once('=')
            .context("container environment entries must have KEY=VALUE form")?;
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            bail!("container environment entries need a non-empty key and no NUL bytes");
        }
    }

    let workspace = Workspace::new(work_dir)?;
    info!(workspace = %workspace.path().display(), "using workspace");
    image::pull_and_unpack(image_ref, workspace.bundle_path())?;
    configure_resolver(workspace.bundle_path())?;

    let pipeline_paths = repository_id
        .map(crate::storage::pipeline_paths)
        .transpose()?;
    if pipeline_paths.is_some() {
        for mountpoint in ["workspace", "artifacts"] {
            fs::create_dir_all(workspace.bundle_path().join("rootfs").join(mountpoint))
                .with_context(|| format!("create image mountpoint /{mountpoint}"))?;
        }
    }
    let config = container::make_runtime_config(
        command,
        pipeline_paths
            .as_ref()
            .map(|(repo, artifacts)| (repo.as_path(), artifacts.as_path())),
        additional_env,
    )?;
    let mut config_file = workspace
        .bundle_dir()
        .create("config.json")
        .context("create OCI runtime config")?;
    config_file
        .write_all(&serde_json::to_vec_pretty(&config)?)
        .context("write OCI runtime config")?;

    let id = container_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("image-runner-{}", std::process::id()));
    let code = container::run_container(workspace.bundle_path(), workspace.state_path(), id)?;
    Ok((
        code,
        pipeline_paths.map(|(_, artifacts)| artifacts.to_string_lossy().into_owned()),
    ))
}

fn configure_resolver(bundle: &Path) -> anyhow::Result<()> {
    let etc = bundle.join("rootfs/etc");
    fs::create_dir_all(&etc).context("create rootfs /etc directory")?;
    let destination = etc.join("resolv.conf");
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(&destination).context("remove image resolver symlink")?;
        }
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => bail!("image /etc/resolv.conf is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect image /etc/resolv.conf"),
    }
    fs::copy("/etc/resolv.conf", &destination)
        .context("copy host DNS resolver configuration into container image")?;
    Ok(())
}
