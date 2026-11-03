//! ComfyUI's upload/queue/poll/download workflow, driven by a workflow the user exported with
//! "Export (API)". Never resubmit a prompt; a cancelled prompt is removed from the queue.
use std::{io::Read, path::Path, sync::Mutex};

use super::{ApiProvider, Duration, Error, MAX_IMAGE, Value, array, error, network_error};
use reqwest::{Client, RequestBuilder, Response, header::HeaderValue};
use serde_json::json;
use url::Url;

const MAX_METADATA: usize = 4 * 1024 * 1024;
const MAX_WORKFLOW: u64 = 1024 * 1024;
const IMAGE: &str = "{{image}}";
const PROMPT: &str = "{{prompt}}";
const SEED: &str = "{{seed}}";

/// Reads `<dir>/<name>.json` and checks it is an API-format graph with `{{image}}` and
/// `{{prompt}}` placeholders. Model IDs cannot contain path separators.
pub fn load_workflow(dir: &Path, name: &str) -> Result<Value, Error> {
    let path = dir.join(format!("{name}.json"));
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_WORKFLOW + 1).read_to_end(&mut bytes))
        .map_err(|_| error(&format!("Could not read the ComfyUI workflow {}", path.display())))?;
    if bytes.len() as u64 > MAX_WORKFLOW {
        return Err(error("The ComfyUI workflow exceeds 1 MiB"));
    }
    let workflow: Value = serde_json::from_slice(&bytes).map_err(|_| error("The ComfyUI workflow is not valid JSON"))?;
    if !workflow.as_object().is_some_and(|nodes| nodes.values().all(|node| node.get("class_type").is_some())) {
        return Err(error("Export the ComfyUI workflow with Export (API); the regular workflow format is not supported"));
    }
    if count(&workflow, IMAGE) == 0 || count(&workflow, PROMPT) == 0 {
        return Err(error("The ComfyUI workflow needs {{image}} in its Load Image node and {{prompt}} in its text prompt"));
    }
    Ok(workflow)
}

fn count(value: &Value, placeholder: &str) -> usize {
    match value {
        Value::String(s) => usize::from(s == placeholder),
        Value::Array(items) => items.iter().map(|item| count(item, placeholder)).sum(),
        Value::Object(map) => map.values().map(|item| count(item, placeholder)).sum(),
        _ => 0,
    }
}

/// Whole-string placeholders only: a prompt containing "{{image}}" is never re-expanded.
fn substitute(value: &Value, image: &str, prompt: &str, seed: u64) -> Value {
    match value {
        Value::String(s) if s == IMAGE => json!(image),
        Value::String(s) if s == PROMPT => json!(prompt),
        Value::String(s) if s == SEED => json!(seed),
        Value::Array(items) => Value::Array(items.iter().map(|item| substitute(item, image, prompt, seed)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), substitute(v, image, prompt, seed))).collect()),
        other => other.clone(),
    }
}

fn seed() -> u64 {
    // Samplers take seeds up to 2^64 - 1; stay within JSON's exact integer range.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    (nanos as u64) & ((1 << 53) - 1)
}

fn base_url(endpoint: &str) -> Result<Url, Error> {
    let mut url = Url::parse(endpoint).map_err(|_| error("Invalid ComfyUI address"))?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn join(base: &Url, path: &str) -> Result<Url, Error> {
    base.join(path).map_err(|_| error("Invalid ComfyUI address"))
}

/// One multipart/form-data upload. The boundary is derived from the image, so it cannot occur in it.
fn upload_body(png: &[u8]) -> (String, String, Vec<u8>) {
    let hash = blake3::hash(png).to_hex();
    let boundary = format!("photocraft-{hash}");
    let name = format!("photocraft-{}.png", &hash.as_str()[..16]);
    let mut body =
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n").into_bytes();
    body.extend_from_slice(png);
    for (field, value) in [("type", "input"), ("overwrite", "true")] {
        body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}").as_bytes());
    }
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, name, body)
}

async fn bytes(response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    if !response.status().is_success() {
        return Err(error(&format!("ComfyUI HTTP {}. Check the address and that ComfyUI is running; no prompt was resubmitted.", response.status().as_u16())));
    }
    body(response, limit).await
}

async fn body(mut response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response.content_length().is_some_and(|size| size > limit as u64) {
        return Err(error("ComfyUI response exceeds the size limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(error("ComfyUI response exceeds the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn metadata(response: Response) -> Result<Value, Error> {
    serde_json::from_slice(&bytes(response, MAX_METADATA).await?).map_err(|_| error("ComfyUI returned invalid JSON"))
}

/// A short identifier from ComfyUI, safe to place in a URL path or an error message.
fn token(value: Option<&Value>, max: usize) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)))
}

/// Validation failures come back as HTTP 400 with a typed error; report only that type.
async fn queue_error(response: Response) -> Error {
    let status = response.status().as_u16();
    let read = async { serde_json::from_slice::<Value>(&body(response, MAX_METADATA).await?).map_err(|_| error("ComfyUI returned invalid JSON")) };
    let detail = tokio::time::timeout(Duration::from_secs(2), read).await.ok().and_then(Result::ok);
    match detail.as_ref().and_then(|value| token(value.pointer("/error/type"), 64)) {
        Some(kind) => error(&format!("ComfyUI rejected the workflow ({kind}). Check its nodes and models in ComfyUI. No layer was added.")),
        None => error(&format!("ComfyUI HTTP {status} while queueing the workflow. No layer was added.")),
    }
}

fn output_image(entry: &Value) -> Result<(String, String, String), Error> {
    let outputs = entry.get("outputs").and_then(Value::as_object);
    let images: Vec<&Value> = outputs.into_iter().flat_map(|nodes| nodes.values()).flat_map(|node| array(node, "images")).collect();
    let of = |kind: &str| -> Vec<&Value> { images.iter().copied().filter(|image| image.get("type").and_then(Value::as_str) == Some(kind)).collect() };
    // Prefer a Save Image result; accept a lone Preview Image when nothing was saved.
    let image = match (of("output").as_slice(), of("temp").as_slice()) {
        ([one], _) | ([], [one]) => *one,
        ([], []) => return Err(error("The ComfyUI workflow finished without an image; add a Save Image node. No layer was added.")),
        _ => return Err(error("The ComfyUI workflow produced several images; save exactly one. No layer was added.")),
    };
    let field = |name: &str| image.get(name).and_then(Value::as_str).filter(|s| s.len() <= 1024).map(str::to_owned);
    match (field("filename"), field("subfolder"), field("type")) {
        (Some(filename), Some(subfolder), Some(kind)) if !filename.is_empty() => Ok((filename, subfolder, kind)),
        _ => Err(error("ComfyUI returned an incomplete image reference; no layer was added")),
    }
}

pub(super) struct Session<'a> {
    pub(super) client: &'a Client,
    pub(super) base: Url,
    pub(super) auth: Option<HeaderValue>,
}

impl Session<'_> {
    pub(super) fn new<'a>(provider: &ApiProvider, client: &'a Client, auth: Option<HeaderValue>) -> Result<Session<'a>, Error> {
        Ok(Session { client, base: base_url(&provider.profile.endpoint)?, auth })
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.auth {
            Some(auth) => request.header("authorization", auth.clone()),
            None => request,
        }
    }

    async fn upload(&self, png: &[u8]) -> Result<String, Error> {
        let (boundary, _, body) = upload_body(png);
        let request = self.client.post(join(&self.base, "upload/image")?).header("content-type", format!("multipart/form-data; boundary={boundary}"));
        let uploaded = metadata(self.authorized(request.body(body)).send().await.map_err(network_error)?).await?;
        let name = uploaded.get("name").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 255);
        match (name, uploaded.get("subfolder").and_then(Value::as_str).unwrap_or_default()) {
            (Some(name), "") => Ok(name.to_owned()),
            (Some(name), subfolder) => Ok(format!("{subfolder}/{name}")),
            (None, _) => Err(error("ComfyUI did not accept the canvas upload")),
        }
    }

    async fn queue(&self, workflow: Value) -> Result<String, Error> {
        let request = self.client.post(join(&self.base, "prompt")?).json(&json!({"prompt":workflow,"client_id":"photocraft"}));
        let response = self.authorized(request).send().await.map_err(network_error)?;
        if !response.status().is_success() {
            return Err(queue_error(response).await);
        }
        let queued = metadata(response).await?;
        token(queued.get("prompt_id"), 128).map(str::to_owned).ok_or_else(|| error("ComfyUI did not return a prompt ID"))
    }

    async fn result(&self, id: &str) -> Result<Vec<u8>, Error> {
        let history = join(&self.base, &format!("history/{id}"))?;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let entries = metadata(self.authorized(self.client.get(history.clone())).send().await.map_err(network_error)?).await?;
            // The history entry appears once the prompt has run, successfully or not.
            let Some(entry) = entries.get(id) else { continue };
            if entry.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
                return Err(error("ComfyUI reported an error while running the workflow; check the ComfyUI console. No layer was added."));
            }
            if entry.pointer("/status/completed").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let (filename, subfolder, kind) = output_image(entry)?;
            let mut view = join(&self.base, "view")?;
            view.query_pairs_mut().append_pair("filename", &filename).append_pair("subfolder", &subfolder).append_pair("type", &kind);
            return bytes(self.authorized(self.client.get(view)).send().await.map_err(network_error)?, MAX_IMAGE).await;
        }
    }

    pub(super) async fn edit(&self, workflow: &Value, png: &[u8], prompt: &str, queued: &Mutex<Option<String>>) -> Result<Vec<u8>, Error> {
        let image = self.upload(png).await?;
        let id = self.queue(substitute(workflow, &image, prompt, seed())).await?;
        *queued.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
        let result = self.result(&id).await;
        // A finished prompt needs no dequeue on a late cancel.
        queued.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        result
    }

    /// Best effort: removes a pending prompt. A prompt that already started keeps running in
    /// ComfyUI, but its result is discarded.
    pub(super) async fn dequeue(&self, id: &str) {
        if let Ok(url) = join(&self.base, "queue") {
            let request = self.authorized(self.client.post(url).json(&json!({"delete":[id]})));
            let _ = tokio::time::timeout(Duration::from_secs(2), request.send()).await;
        }
    }
}

#[cfg(test)]
mod tests;
