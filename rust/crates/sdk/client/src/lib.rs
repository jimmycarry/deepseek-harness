//! Out-of-process JSON-RPC client. Projects the loop; does not reimplement it.

use dsh_sdk_protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Stdio JSON-RPC client over a spawned runtime. Notification frames received
/// while awaiting a response are collected in wire order.
pub struct JsonRpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    notifications: Vec<Value>,
}

impl JsonRpcClient {
    /// Spawn `program` and speak JSON-RPC on stdio.
    pub async fn spawn(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            notifications: Vec::new(),
        })
    }

    /// `initialize` with the SDK route the runtime should use.
    pub async fn initialize(
        &mut self,
        cwd: &str,
        provider: &str,
        model: &str,
    ) -> std::io::Result<Value> {
        self.call(
            methods::INITIALIZE,
            Some(serde_json::json!({
                "cwd": cwd,
                "provider": provider,
                "model": model,
            })),
        )
        .await
    }

    /// `session/prompt`: one text turn on `session_id`. Returns the enqueue
    /// receipt; the turn's notifications land in [`Self::take_notifications`].
    pub async fn prompt(&mut self, session_id: &str, text: &str) -> std::io::Result<Value> {
        self.call(
            methods::SESSION_PROMPT,
            Some(serde_json::json!({
                "sessionId": session_id,
                "contentBlocks": [{ "type": "text", "text": text }],
            })),
        )
        .await
    }

    /// Drain the notifications received so far, in wire order.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.notifications)
    }

    async fn call(&mut self, method: &str, params: Option<Value>) -> std::io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = JsonRpcRequest::new(id, method, params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        loop {
            let mut frame = String::new();
            self.stdout.read_line(&mut frame).await?;
            let Ok(value) = serde_json::from_str::<Value>(&frame) else {
                continue;
            };
            if value.get("id").is_none() {
                self.notifications.push(value);
                continue;
            }
            let parsed: JsonRpcResponse = serde_json::from_value(value)?;
            return Ok(parsed.result.unwrap_or(Value::Null));
        }
    }

    /// Kill the child runtime.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        let _ = self.call(methods::SHUTDOWN, None).await;
        let _ = self.child.kill().await;
        Ok(())
    }
}

/// The last committed assistant text among `session.event` notifications: the
/// SDK's `finalResponse` projection of one turn's notification stream.
pub fn final_assistant_text(notifications: &[Value]) -> Option<String> {
    notifications
        .iter()
        .rev()
        .filter(|frame| frame["method"] == methods::SESSION_EVENT)
        .find_map(|frame| {
            let event = &frame["params"]["event"];
            if event["type"] != "assistant/message" {
                return None;
            }
            let text: String = event["data"]["message"]["content"]
                .as_array()?
                .iter()
                .filter_map(|block| {
                    if block["type"] == "text" {
                        block["text"].as_str()
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sdk_protocol::methods;

    #[test]
    fn projects_the_same_method_names() {
        assert_eq!(methods::SESSION_PROMPT, "session/prompt");
        assert_eq!(methods::SESSION_EVENT, "session.event");
    }

    #[test]
    fn final_assistant_text_reads_the_last_committed_message() {
        let notifications = vec![
            serde_json::json!({
                "method": "session.event",
                "params": { "event": { "type": "turn/start", "data": { "turn": 1 } } },
            }),
            serde_json::json!({
                "method": "session.event",
                "params": { "event": {
                    "type": "assistant/message",
                    "data": { "message": { "content": [
                        { "type": "reasoning", "text": "hidden" },
                        { "type": "text", "text": "SDK snapshot OK" },
                    ] } },
                } },
            }),
            serde_json::json!({
                "method": "session.status",
                "params": { "status": "idle" },
            }),
        ];
        assert_eq!(
            final_assistant_text(&notifications).as_deref(),
            Some("SDK snapshot OK")
        );
        assert_eq!(final_assistant_text(&[]), None);
    }
}
