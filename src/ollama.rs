//! Minimal Ollama HTTP client over the hyper stack already in the tree.
//! The app stays fully useful when Ollama is absent; the caller decides
//! visibility of failures and nothing here ever calls a remote service.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

pub struct OllamaClient {
    pub base_url: String,
    pub probe_timeout_secs: u64,
    pub call_timeout_secs: u64,
}

impl OllamaClient {
    pub fn from_env() -> Self {
        Self {
            base_url: std::env::var("REELVAULT_OLLAMA_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:11434".to_string()),
            probe_timeout_secs: 3,
            call_timeout_secs: 120,
        }
    }

    async fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout_secs: u64,
    ) -> Result<(u16, serde_json::Value), String> {
        let uri: hyper::Uri = format!("{}{}", self.base_url, path)
            .parse()
            .map_err(|e| format!("bad ollama url: {e}"))?;
        let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();

        let send = async {
            let payload = match body {
                Some(v) => Full::new(Bytes::from(
                    serde_json::to_vec(v).map_err(|e| format!("encode: {e}"))?,
                )),
                None => Full::new(Bytes::new()),
            };
            let mut builder = Request::builder().method(method).uri(uri);
            if body.is_some() {
                builder = builder.header("content-type", "application/json");
            }
            let req = builder
                .body(payload)
                .map_err(|e| format!("build request: {e}"))?;
            let resp = client
                .request(req)
                .await
                .map_err(|e| format!("ollama request: {e}"))?;
            let status = resp.status().as_u16();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("ollama body: {e}"))?
                .to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            );
            Ok((status, value))
        };

        tokio::time::timeout(Duration::from_secs(timeout_secs), send)
            .await
            .map_err(|_| format!("ollama timed out after {timeout_secs}s + {}", path))?
    }

    /// Probe availability and return model names from /api/tags.
    pub async fn available(&self) -> Result<Vec<String>, String> {
        let (status, value) = self
            .request_json("GET", "/api/tags", None, self.probe_timeout_secs)
            .await?;
        if status != 200 {
            return Err(format!("ollama /api/tags returned {status}"));
        }
        Ok(value["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Non-streaming generate. The caller supplies the prompt; provenance of
    /// any user reel content is decided above this layer, never here.
    pub async fn generate(&self, model: &str, prompt: &str) -> Result<String, String> {
        let payload = serde_json::json!({"model": model, "prompt": prompt, "stream": false});
        let (status, value) = self
            .request_json(
                "POST",
                "/api/generate",
                Some(&payload),
                self.call_timeout_secs,
            )
            .await?;
        if status != 200 {
            return Err(format!("ollama /api/generate returned {status}"));
        }
        match value["response"].as_str() {
            Some(text) => Ok(text.to_string()),
            None => Err("ollama response missing text".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal HTTP/1.1 stub for POST /api/generate, to prove the client
    /// speaks the real wire format without Ollama installed on this machine.
    async fn stub(listener: TcpListener, status: &str, reply_text: &str) {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut req = String::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            req.push_str(&String::from_utf8_lossy(&buf[..n]));
            if req.contains("\r\n\r\n") {
                break;
            }
        }
        assert!(
            req.starts_with("POST /api/generate"),
            "unexpected request: {req}"
        );
        assert!(req.contains("content-type: application/json"));
        let body_value =
            serde_json::to_vec(&json!({"response": reply_text, "done": true})).unwrap();
        let chunked = status.contains("20");
        let mut resp = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n");
        if chunked {
            resp += &format!(
                "Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
                body_value.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(&body_value).await.unwrap();
            sock.write_all(b"\r\n0\r\n\r\n").await.unwrap();
        } else {
            resp += &format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body_value.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(&body_value).await.unwrap();
        }
    }

    async fn stub_server() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        (listener, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn generate_returns_text_and_handles_chunked_body() {
        let (listener, base) = stub_server().await;
        let h = tokio::spawn(stub(listener, "200 OK", "stub text"));
        let client = OllamaClient {
            base_url: base,
            probe_timeout_secs: 3,
            call_timeout_secs: 5,
        };
        let out = client
            .generate("qwen2.5:3b", "summarize")
            .await
            .expect("generate");
        assert_eq!(out, "stub text");
        h.await.expect("stub");
    }

    #[tokio::test]
    async fn generate_reports_error_status() {
        let (listener, base) = stub_server().await;
        let h = tokio::spawn(stub(listener, "404 Not Found", "nope"));
        let client = OllamaClient {
            base_url: base,
            probe_timeout_secs: 3,
            call_timeout_secs: 5,
        };
        let err = client.generate("m", "p").await.expect_err("404");
        assert!(err.contains("404"));
        h.await.expect("stub");
    }

    #[tokio::test]
    async fn unavailable_when_connection_fails() {
        // Nothing listens here; probe must surface a clean error, not hang.
        let client = OllamaClient {
            base_url: "http://127.0.0.1:1".to_string(),
            probe_timeout_secs: 3,
            call_timeout_secs: 3,
        };
        assert!(client.available().await.is_err());
    }

    #[tokio::test]
    async fn timeout_caps_the_call() {
        let (listener, base) = stub_server().await;
        let h = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            // Read, then never reply before the client times out.
            let mut b = [0u8; 512];
            let _ = s.read(&mut b).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let client = OllamaClient {
            base_url: base,
            probe_timeout_secs: 2,
            call_timeout_secs: 2,
        };
        let err = client.generate("m", "p").await.expect_err("timeout");
        assert!(err.contains("timed out"));
        h.abort();
    }
}
