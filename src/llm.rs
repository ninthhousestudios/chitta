use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{ChittaError, Result};
use crate::synthesis::Llm;

pub struct ClaudeCliLlm {
    pub model: String,
}

impl Default for ClaudeCliLlm {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
        }
    }
}

impl ClaudeCliLlm {
    pub fn new(model: String) -> Self {
        Self { model }
    }
}

impl Llm for ClaudeCliLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut child = Command::new("claude")
            .args([
                "-p",
                "--output-format",
                "text",
                "--model",
                &self.model,
                "--system-prompt",
                system,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ChittaError::Internal(format!("failed to spawn claude CLI: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(user.as_bytes()).await.map_err(|e| {
                ChittaError::Internal(format!("failed to write to claude stdin: {e}"))
            })?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ChittaError::Internal(format!("claude CLI failed: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ChittaError::Internal(format!(
                "claude CLI exited {}: {stderr}",
                output.status
            )));
        }

        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            return Err(ChittaError::Internal(
                "claude CLI returned empty response".into(),
            ));
        }

        Ok(text)
    }
}

#[cfg(feature = "api")]
pub struct ClaudeApiLlm {
    client: reqwest::Client,
    api_key: String,
    pub model: String,
}

#[cfg(feature = "api")]
impl ClaudeApiLlm {
    pub fn from_env(model: String) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ChittaError::Internal("ANTHROPIC_API_KEY not set".into()))?;
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        })
    }
}

#[cfg(feature = "api")]
impl Llm for ClaudeApiLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 16384,
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": user}],
        });

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| ChittaError::Internal(format!("API request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChittaError::Internal(format!(
                "API returned {status}: {text}"
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ChittaError::Internal(format!("failed to parse API response: {e}")))?;

        let text = json["content"][0]["text"]
            .as_str()
            .ok_or_else(|| ChittaError::Internal("no text in API response".into()))?
            .to_string();

        Ok(text)
    }
}
