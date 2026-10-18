use std::{os::fd::AsFd, path::Path};

use anyhow::Context;
use libcontainer::{
    container::builder::ContainerBuilder, syscall::syscall::SyscallType,
    workload::default::DefaultExecutor,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use tracing::info;

pub(crate) fn run_container(bundle: &Path, state_root: &Path, id: String) -> anyhow::Result<i32> {
    info!(container_id = %id, "building libcontainer container");
    let mut container = ContainerBuilder::new(id, SyscallType::default())
        .with_executor(DefaultExecutor {})
        .with_root_path(state_root)?
        .with_stdin(std::io::stdin().as_fd().try_clone_to_owned()?)
        .with_stdout(std::io::stdout().as_fd().try_clone_to_owned()?)
        .with_stderr(std::io::stderr().as_fd().try_clone_to_owned()?)
        .as_init(bundle)
        .with_systemd(false)
        .with_detach(false)
        .build()
        .context(
            "build rootless libcontainer container; rerun with RUST_LOG=libcontainer=debug to identify the namespace or mapping operation denied by the host",
        )?;

    container.start().context("start container")?;
    let pid = container.pid().context("container init has no pid")?;
    info!(
        pid = pid.as_raw(),
        "container started; waiting for command to exit"
    );
    let status = wait_for(pid)?;
    container.delete(true).context("delete container state")?;

    let code = match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, signal, _) => 128 + signal as i32,
        other => anyhow::bail!("unexpected wait status: {other:?}"),
    };
    info!(exit_code = code, "container command exited");
    Ok(code)
}

fn wait_for(pid: Pid) -> anyhow::Result<WaitStatus> {
    loop {
        match waitpid(pid, None).context("wait for container process")? {
            WaitStatus::StillAlive => continue,
            status => return Ok(status),
        }
    }
}
