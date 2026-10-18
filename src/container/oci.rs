use std::{fs, path::Path};

use anyhow::Context;
use libcontainer::oci_spec::runtime::Spec;
use nix::unistd::{getgid, getuid};
use serde_json::{Value, json};

pub(crate) fn make_runtime_config(
    command: &[String],
    pipeline_paths: Option<(&Path, &Path)>,
    additional_env: &[String],
) -> anyhow::Result<Value> {
    let host_uid = getuid().as_raw();
    let host_gid = getgid().as_raw();
    let uid_mappings = id_mappings("/etc/subuid", host_uid)?;
    let gid_mappings = id_mappings("/etc/subgid", host_gid)?;

    // Use the same rootless base configuration as `youki spec --rootless`, then
    // fill in the image rootfs, caller-provided process, and subordinate ID
    // ranges that rootless Docker uses on hosts configured for it.
    let mut config = serde_json::to_value(Spec::rootless(host_uid, host_gid))?;
    config["ociVersion"] = json!("1.1.0");
    config["linux"]["uidMappings"] = json!(uid_mappings);
    config["linux"]["gidMappings"] = json!(gid_mappings);
    let (cwd, mut env) = if pipeline_paths.is_some() {
        (
            "/workspace",
            vec![
                "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
                "HOME=/root".to_owned(),
                "CARGO_HOME=/usr/local/cargo".to_owned(),
                "RUSTUP_HOME=/usr/local/rustup".to_owned(),
                "CARGO_TARGET_DIR=/artifacts".to_owned(),
            ],
        )
    } else {
        (
            "/",
            vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()],
        )
    };
    env.extend(additional_env.iter().cloned());
    config["process"] = json!({
        "terminal": false,
        "user": { "uid": 0, "gid": 0 },
        "args": command,
        "env": env,
        "cwd": cwd,
        "noNewPrivileges": true
    });
    config["root"] = json!({ "path": "rootfs", "readonly": false });
    config["hostname"] = json!("image-runner");
    config["linux"]["cgroupsPath"] = json!(rootless_cgroup_path());
    if let Some((repository, artifacts)) = pipeline_paths {
        let mounts = config["mounts"]
            .as_array_mut()
            .context("OCI runtime config mounts must be an array")?;
        mounts.push(json!({
            "destination": "/workspace",
            "type": "bind",
            "source": repository,
            "options": ["rbind", "rw", "rprivate"]
        }));
        mounts.push(json!({
            "destination": "/artifacts",
            "type": "bind",
            "source": artifacts,
            "options": ["rbind", "rw", "rprivate"]
        }));
    }
    Ok(config)
}

fn rootless_cgroup_path() -> String {
    // An absolute OCI cgroupsPath selects cgroupfs in libcgroups, avoiding the
    // systemd D-Bus manager. Without delegated cgroups, the rootless manager
    // leaves the process in its existing cgroup. This sample sets no limits.
    format!("/ci-with-wasm-{}", std::process::id())
}

fn id_mappings(subid_file: &str, host_id: u32) -> anyhow::Result<Vec<Value>> {
    let username = fs::read_to_string("/etc/passwd").ok().and_then(|passwd| {
        passwd.lines().find_map(|line| {
            let fields: Vec<_> = line.split(':').collect();
            (fields.get(2)?.parse::<u32>().ok()? == host_id)
                .then(|| fields.first().copied().unwrap_or_default().to_owned())
        })
    });
    let mut mappings = vec![json!({
        "containerID": 0,
        "hostID": host_id,
        "size": 1
    })];

    let contents = match fs::read_to_string(subid_file) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(mappings),
        Err(err) => return Err(err).with_context(|| format!("read {subid_file}")),
    };
    let host_id_string = host_id.to_string();
    let mut next_container_id = 1u32;
    for line in contents.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() != 3
            || (fields[0] != host_id_string && username.as_deref() != Some(fields[0]))
        {
            continue;
        }
        let host_start: u32 = fields[1]
            .parse()
            .with_context(|| format!("invalid subordinate ID range in {subid_file}: {line}"))?;
        let size: u32 = fields[2]
            .parse()
            .with_context(|| format!("invalid subordinate ID range in {subid_file}: {line}"))?;
        if size == 0 {
            continue;
        }
        mappings.push(json!({
            "containerID": next_container_id,
            "hostID": host_start,
            "size": size
        }));
        next_container_id = next_container_id
            .checked_add(size)
            .context("subordinate ID mappings exceed u32 range")?;
    }
    Ok(mappings)
}
