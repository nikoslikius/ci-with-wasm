use std::path::Path;

use anyhow::Context;
use extism::{Manifest, PTR, PluginBuilder, UserData, Wasm};
use std::fs;
use tracing::info;

mod cli;
mod protocol;

pub(crate) use cli::run_from_process_args;

pub(crate) struct LoadedPlugin(extism::Plugin);

pub(crate) fn load(plugin_path: &Path, with_wasi: bool) -> anyhow::Result<LoadedPlugin> {
    info!(plugin = %plugin_path.display(), "loading Extism plugin");
    let wasm_bytes = fs::read(plugin_path)
        .with_context(|| format!("read Extism plugin {}", plugin_path.display()))?;
    let manifest = Manifest::new([Wasm::data(wasm_bytes)]);
    let plugin = PluginBuilder::new(manifest)
        .with_wasi(with_wasi)
        .with_function(
            "run_container",
            [PTR],
            [PTR],
            UserData::new(()),
            protocol::run_container,
        )
        .with_function(
            "git_shallow_clone",
            [PTR],
            [PTR],
            UserData::new(()),
            protocol::git_shallow_clone,
        )
        .build()
        .context("load Extism plugin")?;

    Ok(LoadedPlugin(plugin))
}

pub(crate) fn invoke(plugin: &mut LoadedPlugin, input: &str) -> anyhow::Result<String> {
    let output: String = plugin
        .0
        .call("run", input)
        .context("call plugin export `run`")?;
    info!(plugin_output = %output, "Extism plugin completed");
    Ok(output)
}

pub(crate) fn run(plugin_path: &Path, input: &str, with_wasi: bool) -> anyhow::Result<()> {
    let mut plugin = load(plugin_path, with_wasi)?;
    invoke(&mut plugin, input)?;
    Ok(())
}
