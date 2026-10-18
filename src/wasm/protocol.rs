use std::{
    fs::{self, OpenOptions},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use extism::host_fn;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct ContainerRunResult {
    exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_dir: Option<String>,
}

fn run_container_request(input: &str) -> anyhow::Result<ContainerRunResult> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let result_path = std::env::temp_dir().join(format!(
        "ci-with-wasm-result-{}-{timestamp}.json",
        std::process::id()
    ));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&result_path)
        .with_context(|| format!("create worker result file {}", result_path.display()))?;

    let result = (|| {
        let status = Command::new(std::env::current_exe()?)
            .arg("__internal_run")
            .arg(&result_path)
            .arg(input)
            .status()
            .context("start isolated container worker")?;
        if !status.success() {
            bail!("isolated container worker exited with {status}");
        }
        let result = fs::read_to_string(&result_path)
            .with_context(|| format!("read worker result {}", result_path.display()))?;
        serde_json::from_str(&result).context("parse worker result")
    })();
    let _ = fs::remove_file(&result_path);
    result
}

host_fn!(pub(super) run_container(_user_data: (); request: String) -> String {
    let result = run_container_request(&request)
        .map_err(|error| extism::Error::msg(format!("{error:#}")))?;
    serde_json::to_string(&result).map_err(|error| extism::Error::msg(error.to_string()))
});

#[derive(Debug, Deserialize)]
struct GitCloneRequest {
    url: String,
}

#[derive(Debug, Serialize)]
struct GitCloneResult {
    repository_id: String,
}

fn git_shallow_clone_request(input: &str) -> anyhow::Result<GitCloneResult> {
    let request: GitCloneRequest =
        serde_json::from_str(input).context("parse git clone request")?;
    let repository_id = crate::storage::create_repository_id()?;
    let destination = crate::storage::repository_storage_root()?.join(&repository_id);
    crate::git::shallow_clone(&request.url, &destination)?;
    Ok(GitCloneResult { repository_id })
}

host_fn!(pub(super) git_shallow_clone(_user_data: (); request: String) -> String {
    let result = git_shallow_clone_request(&request)
        .map_err(|error| extism::Error::msg(format!("{error:#}")))?;
    serde_json::to_string(&result).map_err(|error| extism::Error::msg(error.to_string()))
});
