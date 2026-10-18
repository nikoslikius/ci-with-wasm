# ci-with-wasm

Programmable Continuous Integration system with WebAssembly runtime.

Pipelines are **WebAssembly plugins** ([Extism](https://extism.org)) that call host functions: shallow-clone a Git repository, and run a command in a **rootless OCI container** ([Youki `libcontainer`](https://github.com/youki-dev/youki)). A built-in webhook server can trigger a plugin on every GitHub `push`.

## How it works

```
GitHub push ──► webhook server ──► WASM plugin (run)
                              
                                             ├─ git_shallow_clone ─► checkout (depth 1)
                                             └─ run_container ─────► rootless container
                                                                      /workspace  (checkout)
                                                                      /artifacts  (persistent)
```

The plugin owns the pipeline logic (which commands, in which order, what to do on failure). The host only provides capabilities.

## Requirements

- Linux with unprivileged user namespaces enabled. No `sudo`, Docker, or daemon is used.
- For Docker-like UID/GID mapping: ranges in `/etc/subuid` and `/etc/subgid`, plus `newuidmap`/`newgidmap`.
- Rust toolchain with edition 2024 support, plus native build dependencies of `libcontainer` (libseccomp and libsystemd development files on most distributions).
- [`just`](https://github.com/casey/just), `curl`, `sha256sum`, `tar` for the Python plugin recipes.
- The Python PDK setup recipe supports **x86_64 Linux** only.

## Quick start

```sh
# Build the example plugin (downloads and verifies the Python PDK on first run)
just python-build

# Run it once against a public repository
just python-run

# Or start the webhook server
export GITHUB_WEBHOOK_SECRET='your-webhook-secret'
just python-serve
```

Quickstart uses plugin(WASM module) from `./examples/python`.

The first run downloads the pinned Extism Python PDK and Binaryen into `target/`, checks their SHA-256 sums, and compiles the plugin. Startup of the server can take a while in debug builds because the plugin is compiled when it is loaded.

## Command line

```
ci-with-wasm --plugin <WASM> [options]
```

| Flag | Default | Description |
| --- | --- | --- |
| `--plugin <WASM>` | required | Extism module exporting `run` |
| `--plugin-input <JSON>` | `{}` | Input for a single `run` call |
| `--plugin-wasi` | off | Enable WASI (required by Python PDK plugins) |
| `--serve` | off | Run the webhook server instead of a single call |
| `--listen <ADDR>` | `127.0.0.1:8080` | Webhook listen address |
| `--webhook-secret-env <ENV>` | `GITHUB_WEBHOOK_SECRET` | Name of the env var holding the secret |
| `--webhook-image <IMAGE>` | `rust:1.90-alpine` | `pipeline.image` sent to the plugin |
| `--webhook-command <JSON>` | `["cargo","+1.90.0","build"]` | `pipeline.command` (JSON argv) sent to the plugin |

## Host functions

Plugins import these from `extism:host/user`. Both take and return JSON strings.

**`git_shallow_clone`**

| | |
| --- | --- |
| Request | `{"url": "https://github.com/owner/repo.git"}` |
| Response | `{"repository_id": "..."}` |

Makes a depth-1 clone under the project state directory.

**`run_container`**

| Request field | Required | Description |
| --- | --- | --- |
| `image` | yes | OCI image reference, e.g. `rust:1.90-alpine` |
| `command` | yes | argv array (no implicit shell; use `["/bin/sh","-c","..."]` if needed) |
| `repository_id` | no | Mounts that checkout read-write at `/workspace` and a per-repository artifact directory at `/artifacts` |
| `env` | no | Array of `KEY=VALUE` strings |
| `id` | no | Container ID (generated if omitted) |

Unknown fields are rejected. The response is `{"exit_code": <int>, "artifact_dir": "<host path>"}`; `artifact_dir` is present when `repository_id` was given.

With a `repository_id`, the command runs in `/workspace` with `CARGO_TARGET_DIR=/artifacts`, so build output survives on the host. The image's `ENTRYPOINT`, `CMD`, environment, and working directory are ignored.

## Writing a plugin

A plugin exports `run`, reads JSON input, and returns a JSON result. The included Python example (`examples/python/plugin.py`) does the following:

1. Takes `pipeline.repository_id`, or clones `pipeline.git_url`, or falls back to `github.payload.repository.clone_url`.
2. Runs a `COMMANDS` list defined in the plugin source (`cargo check`, then `cargo build`), in order, in the same checkout and artifact directory.
3. Stops at the first non-zero exit code.
4. Returns `repository_id`, `success`, `executed_commands`, `total_commands`, per-step `steps` (with `exit_code` and `artifact_dir`), and `delivery_id` when present.

Note that the example uses `pipeline.image` and `pipeline.env` from its input, but **ignores `pipeline.command`** (`--webhook-command`); its commands come from `COMMANDS`.

Manual run with explicit input:

```sh
ci-with-wasm --plugin target/wasm-python/python.wasm --plugin-wasi \
  --plugin-input '{"pipeline":{"git_url":"https://github.com/dtolnay/anyhow.git","image":"rust:1.90-alpine"}}'
```

## Webhook server

`--serve` keeps the plugin loaded and listens on `127.0.0.1:8080` for `POST /github/webhook`. Configure GitHub to send **push** events there with the same secret.

For each valid push, the plugin receives:

```json
{
  "pipeline": { "git_url": "<repository.clone_url>", "image": "...", "command": ["..."] },
  "github":   { "event": "push", "delivery_id": "...", "payload": { } }
}
```

| Response | When |
| --- | --- |
| `202` accepted | Valid push queued |
| `202` ignored | Event type other than `push`, or a duplicate delivery ID |
| `400` | Payload is not valid JSON or has no `repository.clone_url` |
| `401` | `X-Hub-Signature-256` missing or invalid |
| `413` | Body larger than 2 MiB |
| `503` | Queue full (16 pending) or worker unavailable |

Behavior to be aware of:

- `202` means accepted, not built. Results and failures appear only in the server log.
- One worker runs pipelines serially. The queue is in memory, so stopping the server drops pending deliveries.
- Delivery IDs are deduplicated for the lifetime of the process, and only after they were queued successfully, so a retry after `503` is accepted.
- The server speaks plain HTTP. For public use, put a TLS-terminating reverse proxy in front of it.

### Private repositories

Set `GITHUB_TOKEN` (read access to repository contents) in the server's environment. It is used only for HTTPS clones on `github.com`, is passed to Gitoxide directly, and does not appear in plugin input or logs. Credential helpers and the GitHub CLI are not invoked. Without a token, public repositories clone anonymously and private ones fail with a configuration hint.

## Storage and caching

| What | Where |
| --- | --- |
| Run bundles, runtime state, checkouts, artifacts | Per-user project state directory (on Linux, `$XDG_STATE_HOME/ci-with-wasm/`; runs under `runs/`) |
| Pulled images | Per-user project cache directory, `images/` subdirectory |

- Images are cached by exact reference for 24 hours. Delete the `images` directory to force a fresh pull.
- Every run gets its own writable rootfs copied from the cached image.
- The `Justfile` redirects `XDG_STATE_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME` to `target/wasm-*`, so recipes don't touch your real directories. Running the binary directly uses the real ones.

## Logging

`tracing` at `info` by default. Use `RUST_LOG` to change it, for example `RUST_LOG=libcontainer=debug` for namespace and ID-mapping diagnostics.

## Project layout

| Path | Purpose |
| --- | --- |
| `src/wasm/` | CLI, plugin loading, host function protocol |
| `src/server/webhook/` | HTTP server, signature check, queue, worker |
| `src/runner/` | Orchestrates one container run |
| `src/image/` | Image pull, cache, rootfs copy |
| `src/container/` | OCI config and `libcontainer` runtime |
| `src/git/` | Shallow clone via Gitoxide |
| `src/storage/` | Workspace, repository, and artifact paths |
| `examples/python/` | Example Python plugin |
| `Justfile` | Plugin build and run recipes |

## AI Disclosure

This project was built with help of LLMs.
