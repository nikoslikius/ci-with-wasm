use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::Parser;

use super::{invoke, load, run};

#[derive(Debug, Parser)]
#[command(about = "Run a WebAssembly plugin with the container management host capability")]
struct Args {
    /// Extism plugin module; it must export `run`
    #[arg(long, required = true, value_name = "WASM")]
    plugin: PathBuf,

    /// JSON input passed to the plugin's `run` export
    #[arg(long, default_value = "{}", value_name = "JSON")]
    plugin_input: String,

    /// Enable WASI for plugins built with the Extism Python PDK
    #[arg(long)]
    plugin_wasi: bool,

    /// Listen for signed GitHub push webhooks and invoke this plugin for each delivery
    #[arg(long)]
    serve: bool,

    /// HTTP address for webhook requests
    #[arg(long, default_value = "127.0.0.1:8080", value_name = "ADDRESS")]
    listen: String,

    /// Name of the environment variable containing the GitHub webhook secret
    #[arg(long, default_value = "GITHUB_WEBHOOK_SECRET", value_name = "ENV")]
    webhook_secret_env: String,

    /// Container image used by the pipeline input generated for each push
    #[arg(long, default_value = "rust:1.90-alpine", value_name = "IMAGE")]
    webhook_image: String,

    /// JSON argv array for the container command generated for each push
    #[arg(
        long,
        default_value = "[\"cargo\",\"+1.90.0\",\"build\"]",
        value_name = "JSON"
    )]
    webhook_command: String,
}

pub(crate) fn run_from_process_args() -> Result<ExitCode> {
    let args = Args::parse();
    if args.serve {
        let secret = std::env::var(&args.webhook_secret_env).map_err(|_| {
            anyhow::anyhow!(
                "webhook secret environment variable {} is not set",
                args.webhook_secret_env
            )
        })?;
        if secret.is_empty() {
            anyhow::bail!(
                "webhook secret environment variable {} is empty",
                args.webhook_secret_env
            );
        }
        let command: Vec<String> = serde_json::from_str(&args.webhook_command)
            .context("parse --webhook-command as a JSON string array")?;
        if command.is_empty() {
            anyhow::bail!("--webhook-command must contain at least one argv item");
        }
        let mut plugin = load(&args.plugin, args.plugin_wasi)?;
        crate::server::serve(
            &args.listen,
            &secret,
            &args.webhook_image,
            &command,
            move |input| invoke(&mut plugin, input),
        )?;
    } else {
        run(&args.plugin, &args.plugin_input, args.plugin_wasi)?;
    }
    Ok(ExitCode::SUCCESS)
}
