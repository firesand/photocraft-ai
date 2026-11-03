use super::*;
use crate::api::{EditRequest, EditResult, ImageProvider, Profile};
use crate::api_config::Protocol;
use std::{
    io::Write,
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

fn mock(responses: Vec<(u16, Vec<u8>)>) -> (String, std::thread::JoinHandle<Vec<String>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (code, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(Instant::now() < deadline, "missing expected ComfyUI request");
                std::thread::sleep(Duration::from_millis(2));
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8_lossy(&bytes).into_owned());
            seen.fetch_add(1, Ordering::Release);
            write!(stream, "HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            let _ = stream.write_all(&body);
        }
        requests
    });
    (base, thread, count)
}

fn workflow() -> Value {
    json!({
        "3": {"class_type":"LoadImage","inputs":{"image":"{{image}}"}},
        "6": {"class_type":"CLIPTextEncode","inputs":{"text":"{{prompt}}","clip":["4",1]}},
        "9": {"class_type":"KSampler","inputs":{"seed":"{{seed}}","denoise":0.6}},
        "10": {"class_type":"SaveImage","inputs":{"images":["8",0],"filename_prefix":"photocraft"}}
    })
}

fn provider(base: &str, key: &str) -> ApiProvider {
    let profile = Profile { protocol: Protocol::ComfyUi, endpoint: base.into(), model: "edit".into() };
    ApiProvider::new(profile, key.into()).unwrap().with_workflow(workflow())
}

fn uploaded() -> (u16, Vec<u8>) {
    (200, br#"{"name":"photocraft-0123.png","subfolder":"","type":"input"}"#.to_vec())
}
fn queued() -> (u16, Vec<u8>) {
    (200, br#"{"prompt_id":"p-1","number":3,"node_errors":{}}"#.to_vec())
}
fn running() -> (u16, Vec<u8>) {
    (200, b"{}".to_vec())
}
fn finished(outputs: Value) -> (u16, Vec<u8>) {
    (200, json!({"p-1":{"status":{"status_str":"success","completed":true},"outputs":outputs}}).to_string().into_bytes())
}
fn saved(name: &str, kind: &str) -> Value {
    json!({"images":[{"filename":name,"subfolder":"","type":kind}]})
}

fn edit(provider: &ApiProvider, cancelled: &dyn Fn() -> bool) -> (tempfile::TempDir, Result<EditResult, Error>) {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("canvas.png");
    std::fs::write(&input, super::super::tests::png()).unwrap();
    let result = provider.edit(&EditRequest { image: &input, prompt: "Make it yellow", workspace: work.path() }, cancelled);
    (work, result)
}

fn body(request: &str) -> &str {
    request.split_once("\r\n\r\n").unwrap().1
}

#[test]
fn uploads_queues_polls_and_downloads_the_saved_image() {
    let image = super::super::tests::png();
    let (base, server, _) = mock(vec![uploaded(), queued(), running(), finished(json!({"10":saved("out_00001_.png","output")})), (200, image.clone())]);
    let (_work, result) = edit(&provider(&base, ""), &|| false);
    assert_eq!(std::fs::read(result.unwrap().saved_path).unwrap(), image);
    let requests = server.join().unwrap();
    let lines: Vec<&str> = requests.iter().map(|r| r.lines().next().unwrap()).collect();
    assert_eq!(
        lines,
        [
            "POST /upload/image HTTP/1.1",
            "POST /prompt HTTP/1.1",
            "GET /history/p-1 HTTP/1.1",
            "GET /history/p-1 HTTP/1.1",
            "GET /view?filename=out_00001_.png&subfolder=&type=output HTTP/1.1"
        ]
    );
    assert!(requests.iter().all(|r| !r.to_ascii_lowercase().contains("authorization")), "no key, no credential header");
    assert!(requests[0].contains("multipart/form-data; boundary=photocraft-"));
    assert!(requests[0].contains("name=\"type\"\r\n\r\ninput"));
    let queued: Value = serde_json::from_str(body(&requests[1])).unwrap();
    let graph = &queued["prompt"];
    assert_eq!(graph["3"]["inputs"]["image"], "photocraft-0123.png");
    // Diffusion prompts get the instruction alone, without the canvas-preservation sentence.
    assert_eq!(graph["6"]["inputs"]["text"], "Make it yellow");
    assert!(graph["9"]["inputs"]["seed"].is_u64());
    assert_eq!(graph["9"]["inputs"]["denoise"], 0.6);
    assert_eq!(queued["client_id"], "photocraft");
}

#[test]
fn a_key_is_sent_as_a_bearer_token_on_every_request() {
    let image = super::super::tests::png();
    let (base, server, _) = mock(vec![uploaded(), queued(), finished(json!({"10":saved("out.png","output")})), (200, image)]);
    let (_work, result) = edit(&provider(&base, "proxy-token"), &|| false);
    result.unwrap();
    for request in server.join().unwrap() {
        assert!(request.to_ascii_lowercase().contains("authorization: bearer proxy-token"));
    }
}

#[test]
fn a_lone_preview_is_accepted_but_several_or_no_images_are_not() {
    let image = super::super::tests::png();
    let (base, server, _) = mock(vec![uploaded(), queued(), finished(json!({"12":saved("preview.png","temp")})), (200, image)]);
    let (_work, result) = edit(&provider(&base, ""), &|| false);
    result.unwrap();
    assert!(server.join().unwrap()[3].contains("type=temp"));

    for outputs in [json!({"10":saved("a.png","output"),"11":saved("b.png","output")}), json!({})] {
        let (base, server, _) = mock(vec![uploaded(), queued(), finished(outputs)]);
        let (work, result) = edit(&provider(&base, ""), &|| false);
        assert!(result.err().unwrap().to_string().contains("No layer was added"));
        assert!(!work.path().join("api-result.png").exists());
        server.join().unwrap();
    }
}

#[test]
fn execution_and_validation_errors_report_no_provider_text() {
    let failed = (200, br#"{"p-1":{"status":{"status_str":"error","completed":false},"outputs":{}}}"#.to_vec());
    let (base, server, _) = mock(vec![uploaded(), queued(), failed]);
    let (_work, result) = edit(&provider(&base, ""), &|| false);
    assert!(result.err().unwrap().to_string().contains("ComfyUI console"));
    server.join().unwrap();

    let rejected = json!({"error":{"type":"prompt_outputs_failed_validation","message":"PRIVATE prompt text","details":"PRIVATE"},"node_errors":{}});
    let (base, server, _) = mock(vec![uploaded(), (400, rejected.to_string().into_bytes())]);
    let (_work, result) = edit(&provider(&base, ""), &|| false);
    let message = result.err().unwrap().to_string();
    assert!(message.contains("prompt_outputs_failed_validation"));
    assert!(!message.contains("PRIVATE"));
    server.join().unwrap();
}

#[test]
fn cancelling_while_queued_removes_the_prompt_from_the_queue() {
    let (base, server, seen) = mock(vec![uploaded(), queued(), running(), (200, b"{}".to_vec())]);
    // Cancel once the first history poll was answered: the prompt is still in ComfyUI's queue.
    let (work, result) = edit(&provider(&base, ""), &|| seen.load(Ordering::Acquire) >= 3);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!work.path().join("api-result.png").exists());
    let requests = server.join().unwrap();
    assert!(requests[3].starts_with("POST /queue HTTP/1.1"));
    assert_eq!(serde_json::from_str::<Value>(body(&requests[3])).unwrap(), json!({"delete":["p-1"]}));
}

#[test]
fn workflows_need_the_api_format_and_both_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, value: &Value| std::fs::write(dir.path().join(format!("{name}.json")), value.to_string()).unwrap();
    write("good", &workflow());
    assert_eq!(load_workflow(dir.path(), "good").unwrap(), workflow());
    write("no-prompt", &json!({"3":{"class_type":"LoadImage","inputs":{"image":"{{image}}"}}}));
    assert!(load_workflow(dir.path(), "no-prompt").unwrap_err().to_string().contains("{{prompt}}"));
    // The UI's own save format has a nodes array instead of class_type entries.
    write("ui-format", &json!({"nodes":[{"type":"LoadImage","widgets_values":["{{image}}","{{prompt}}"]}],"links":[]}));
    assert!(load_workflow(dir.path(), "ui-format").unwrap_err().to_string().contains("Export (API)"));
    std::fs::write(dir.path().join("broken.json"), b"{").unwrap();
    assert!(load_workflow(dir.path(), "broken").is_err());
    assert!(load_workflow(dir.path(), "missing").unwrap_err().to_string().contains("missing.json"));
}

#[test]
fn placeholders_are_replaced_only_as_whole_strings() {
    let graph = json!({"1":{"class_type":"X","inputs":{"a":"{{prompt}}","b":"keep {{prompt}} literal","c":["{{image}}",2],"d":"{{seed}}"}}});
    let out = substitute(&graph, "in.png", "say {{image}}", 42);
    assert_eq!(out["1"]["inputs"]["a"], "say {{image}}", "a prompt is never re-expanded");
    assert_eq!(out["1"]["inputs"]["b"], "keep {{prompt}} literal");
    assert_eq!(out["1"]["inputs"]["c"], json!(["in.png", 2]));
    assert_eq!(out["1"]["inputs"]["d"], 42);
    // A reverse-proxy path prefix is kept when joining endpoint paths.
    assert_eq!(join(&base_url("http://127.0.0.1:8188/comfy").unwrap(), "prompt").unwrap().as_str(), "http://127.0.0.1:8188/comfy/prompt");
}
