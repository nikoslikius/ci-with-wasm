use std::{
    collections::HashSet,
    io::Read,
    sync::mpsc::{self, SyncSender, TrySendError},
    thread,
};

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use tracing::{error, info};

const MAX_WEBHOOK_BYTES: usize = 2 * 1024 * 1024;
const PIPELINE_QUEUE_CAPACITY: usize = 16;
type HmacSha256 = Hmac<Sha256>;

struct PipelineJob {
    delivery_id: String,
    input: String,
}

/// Serve authenticated GitHub push webhooks and queue pipelines for background execution.
pub(crate) fn serve(
    listen: &str,
    secret: &str,
    image: &str,
    command: &[String],
    mut execute: impl FnMut(&str) -> Result<String> + Send + 'static,
) -> Result<()> {
    let server = Server::http(listen)
        .map_err(|err| anyhow::anyhow!("bind webhook server at {listen}: {err}"))?;
    info!(address = %listen, "GitHub webhook server listening");
    let mut seen_deliveries = HashSet::new();
    let (pipeline_tx, pipeline_rx) = mpsc::sync_channel::<PipelineJob>(PIPELINE_QUEUE_CAPACITY);
    thread::Builder::new()
        .name("webhook-pipeline-worker".to_owned())
        .spawn(move || {
            while let Ok(job) = pipeline_rx.recv() {
                match execute(&job.input) {
                    Ok(output) => info!(
                        delivery_id = %job.delivery_id,
                        pipeline_output = %output,
                        "webhook pipeline completed"
                    ),
                    Err(err) => error!(
                        delivery_id = %job.delivery_id,
                        error = %format_args!("{err:#}"),
                        "webhook pipeline failed"
                    ),
                }
            }
        })
        .context("start webhook pipeline worker")?;

    for request in server.incoming_requests() {
        if let Err(err) = handle_request(
            request,
            secret,
            image,
            command,
            &mut seen_deliveries,
            &pipeline_tx,
        ) {
            error!(error = %format_args!("{err:#}"), "failed to handle webhook request");
        }
    }
    Ok(())
}

fn handle_request(
    mut request: Request,
    secret: &str,
    image: &str,
    command: &[String],
    seen_deliveries: &mut HashSet<String>,
    pipeline_tx: &SyncSender<PipelineJob>,
) -> Result<()> {
    if request.method() != &Method::Post || request.url() != "/github/webhook" {
        return respond(request, 404, "not found");
    }

    let signature = header(&request, "X-Hub-Signature-256").map(str::to_owned);
    let event = header(&request, "X-GitHub-Event").map(str::to_owned);
    let delivery_id = header(&request, "X-GitHub-Delivery")
        .unwrap_or("unknown")
        .to_owned();

    let mut body = Vec::new();
    request
        .as_reader()
        .take((MAX_WEBHOOK_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .context("read webhook request body")?;
    if body.len() > MAX_WEBHOOK_BYTES {
        return respond(request, 413, "webhook payload too large");
    }
    if !verify_signature(secret, signature.as_deref(), &body) {
        return respond(request, 401, "invalid webhook signature");
    }

    if event.as_deref() != Some("push") {
        info!(
            event = event.as_deref().unwrap_or("missing"),
            "ignoring unsupported GitHub event"
        );
        return respond(request, 202, "event ignored");
    }
    if delivery_id != "unknown" && seen_deliveries.contains(&delivery_id) {
        info!(delivery_id, "ignoring duplicate GitHub webhook delivery");
        return respond(request, 202, "duplicate delivery ignored");
    }

    let payload: Value = match serde_json::from_slice(&body).context("parse GitHub push payload") {
        Ok(payload) => payload,
        Err(err) => return respond(request, 400, &format!("invalid JSON payload: {err:#}")),
    };
    let Some(git_url) = payload
        .get("repository")
        .and_then(|repository| repository.get("clone_url"))
        .and_then(Value::as_str)
    else {
        return respond(request, 400, "push payload has no repository.clone_url");
    };

    let input = json!({
        "pipeline": {
            "git_url": git_url,
            "image": image,
            "command": command,
        },
        "github": {
            "event": event.as_deref().unwrap_or("push"),
            "delivery_id": delivery_id,
            "payload": payload,
        },
    })
    .to_string();

    let job = PipelineJob {
        delivery_id: delivery_id.clone(),
        input,
    };
    match pipeline_tx.try_send(job) {
        Ok(()) => {
            if delivery_id != "unknown" {
                seen_deliveries.insert(delivery_id.clone());
            }
            info!(
                delivery_id,
                repository = git_url,
                "queued pipeline for GitHub push"
            );
            let response = json!({"status":"accepted", "delivery_id":delivery_id});
            respond(request, 202, &response.to_string())
        }
        Err(TrySendError::Full(_)) => respond(request, 503, "pipeline queue is full; retry later"),
        Err(TrySendError::Disconnected(_)) => {
            respond(request, 503, "pipeline worker is unavailable")
        }
    }
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn verify_signature(secret: &str, signature: Option<&str>, body: &[u8]) -> bool {
    let Some(signature) = signature.and_then(|value| value.strip_prefix("sha256=")) else {
        return false;
    };
    if signature.len() != 64 {
        return false;
    }
    let Ok(signature) = decode_hex(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        anyhow::bail!("invalid hex length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hex = std::str::from_utf8(pair).context("signature is not ASCII")?;
            u8::from_str_radix(hex, 16).context("signature is not valid hexadecimal")
        })
        .collect()
}

fn respond(request: Request, status: u16, body: &str) -> Result<()> {
    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
        .expect("static response header is valid");
    request
        .respond(
            Response::from_string(body.to_owned())
                .with_status_code(StatusCode(status))
                .with_header(content_type),
        )
        .context("send webhook response")
}
