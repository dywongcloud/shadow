//! Integration test: the REAL `shadw` binary against a real local HTTP server.
//!
//! A tiny tokio TCP shim records each request (method, path, headers, body) and
//! returns canned responses. We then run the actual compiled CLI against it and
//! assert the wire contract: bearer auth, team header, method/path, JSON body,
//! output, and error handling. This exercises reqwest's real sending path 1:1 —
//! no mocking of the client.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

#[derive(Clone, Debug)]
struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Start the shim. Returns (base_url, captured-requests handle).
async fn start_shim() -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let cap = cap.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read until end of headers.
                let header_end = loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                break pos + 4;
                            }
                        }
                        Err(_) => return,
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut content_len = 0usize;
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        let (k, v) = (k.trim().to_string(), v.trim().to_string());
                        if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.parse().unwrap_or(0);
                        }
                        headers.push((k, v));
                    }
                }
                // Read the rest of the body.
                let mut body = buf[header_end..].to_vec();
                while body.len() < content_len {
                    match sock.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                let body = String::from_utf8_lossy(&body).to_string();
                cap.lock().unwrap().push(Captured { method, path: path.clone(), headers, body });

                // Canned response: 404 for /v1/missing, else 200 JSON.
                let (status, payload) = if path.starts_with("/v1/missing") {
                    ("404 Not Found", "not found".to_string())
                } else {
                    ("200 OK", r#"{"ok":true}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (format!("http://{addr}"), captured)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn shadw() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_shadw"));
    // Isolate from any real ~/.shadw config so tests are deterministic.
    c.env("HOME", std::env::temp_dir().join("shadw-itest-home"));
    c
}

#[tokio::test]
async fn get_sends_bearer_and_team_headers_and_parses_json() {
    let (url, cap) = start_shim().await;
    let out = shadw()
        .args(["--api", &url, "--token", "hive_testkey", "--team", "acme", "--json", "obs", "overview"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"ok\""), "parsed+printed JSON body, got: {stdout}");

    let reqs = cap.lock().unwrap().clone();
    let r = reqs.iter().find(|r| r.path == "/v1/overview").expect("overview request hit the server");
    assert_eq!(r.method, "GET");
    assert_eq!(r.header("authorization"), Some("Bearer hive_testkey"), "API key sent as bearer");
    assert_eq!(r.header("x-hive-team"), Some("acme"), "team scoping header sent");
}

#[tokio::test]
async fn post_deploy_sends_json_body_with_fields_and_env() {
    let (url, cap) = start_shim().await;
    let out = shadw()
        .args([
            "--api", &url, "--token", "hive_k",
            "deploy", "https://github.com/a/b", "--branch", "main", "--target", "production", "--no-cache", "-e", "FOO=bar",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let reqs = cap.lock().unwrap().clone();
    let r = reqs.iter().find(|r| r.path == "/v1/git/deploy").expect("deploy request hit the server");
    assert_eq!(r.method, "POST");
    assert!(r.header("content-type").unwrap_or("").contains("application/json"));
    let body: serde_json::Value = serde_json::from_str(&r.body).expect("valid JSON body");
    assert_eq!(body["repo_url"], "https://github.com/a/b");
    assert_eq!(body["branch"], "main");
    assert_eq!(body["target"], "production");
    assert_eq!(body["use_cache"], false);
    assert_eq!(body["env"]["FOO"], "bar");
}

#[tokio::test]
async fn non_2xx_is_a_clear_error_and_nonzero_exit() {
    let (url, cap) = start_shim().await;
    let out = shadw()
        .args(["--api", &url, "--token", "hive_k", "api", "GET", "/v1/missing"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "non-2xx must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HTTP 404"), "error names the status, got: {stderr}");

    let reqs = cap.lock().unwrap().clone();
    assert!(reqs.iter().any(|r| r.path == "/v1/missing" && r.method == "GET"));
}

#[tokio::test]
async fn delete_uses_delete_method() {
    let (url, cap) = start_shim().await;
    let out = shadw()
        .args(["--api", &url, "--token", "hive_k", "keys", "revoke", "key_123"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = cap.lock().unwrap().clone();
    let r = reqs.iter().find(|r| r.path == "/v1/apikeys/key_123").expect("revoke hit");
    assert_eq!(r.method, "DELETE");
}
