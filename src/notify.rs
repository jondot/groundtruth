//! Notifiers: deliver check results to external sinks.

use anyhow::Context;
use reqwest::Client;
use serde_json::json;
use tokio::time::{Duration, sleep};
use tracing::warn;

/// A single alert payload.
pub struct Notification {
    pub check: String,
    pub status: String, // FAIL | ERROR | RECOVERED
    pub detail: String,
}

/// A configured notifier sink.
#[derive(Debug)]
pub enum Notifier {
    Webhook { url: String },
}

impl Notifier {
    /// POST the notification with up to 3 attempts. Returns Err only after all attempts fail.
    pub async fn send(&self, n: &Notification) -> anyhow::Result<()> {
        let Notifier::Webhook { url } = self;
        let emoji = if n.status == "RECOVERED" {
            ":white_check_mark:"
        } else {
            ":red_circle:"
        };
        let body = json!({
            "text": format!("{emoji} {} {}: {}", n.check, n.status, n.detail),
            "check": n.check,
            "status": n.status,
            "detail": n.detail,
        });
        let client = Client::new();
        let delays = [0u64, 200, 400];
        let mut last_err = anyhow::anyhow!("no attempts made");
        for (i, &delay_ms) in delays.iter().enumerate() {
            if delay_ms > 0 {
                sleep(Duration::from_millis(delay_ms)).await;
            }
            match client
                .post(url)
                .json(&body)
                .send()
                .await
                .context("webhook request failed")
                .and_then(|r| r.error_for_status().context("webhook returned non-2xx"))
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    warn!(attempt = i + 1, err = %last_err, "webhook attempt failed");
                }
            }
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Accept one connection, reply with `response_status`, return the raw request.
    async fn capture_server(response_status: u16) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\n\r\n",
                status = response_status,
                reason = if response_status == 200 {
                    "OK"
                } else {
                    "Internal Server Error"
                },
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            request
        });

        (url, handle)
    }

    /// Like `capture_server` but accepts N connections.
    async fn capture_server_multi(
        response_status: u16,
        times: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            let mut captured = Vec::new();
            for _ in 0..times {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                captured.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\n\r\n",
                    status = response_status,
                    reason = if response_status == 200 {
                        "OK"
                    } else {
                        "Internal Server Error"
                    },
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
            }
            captured
        });

        (url, handle)
    }

    #[tokio::test]
    async fn webhook_posts_universal_payload() {
        let (url, server) = capture_server(200).await;
        let notifier = Notifier::Webhook { url };
        let n = Notification {
            check: "db_ping".into(),
            status: "FAIL".into(),
            detail: "timeout".into(),
        };
        notifier.send(&n).await.unwrap();
        let captured = server.await.unwrap();
        assert!(captured.contains("\"text\""), "universal text key missing");
        assert!(captured.contains("db_ping"), "check name missing from body");
        assert!(captured.contains("FAIL"), "status missing from body");
        assert!(captured.contains("timeout"), "detail missing from body");
    }

    #[tokio::test]
    async fn non_2xx_eventually_errors() {
        // 500 on all 3 attempts.
        let (url, server) = capture_server_multi(500, 3).await;
        let notifier = Notifier::Webhook { url };
        let n = Notification {
            check: "test".into(),
            status: "FAIL".into(),
            detail: "oops".into(),
        };
        let result = notifier.send(&n).await;
        let requests = server.await.unwrap();
        assert!(result.is_err(), "expected Err after all retries fail");
        assert_eq!(requests.len(), 3, "expected exactly 3 attempts");
    }
}
