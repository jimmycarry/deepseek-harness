//! Out-of-process JSON-RPC client. Projects the loop; does not reimplement it.
//!
//! [`JsonRpcClient`] is the thin stdio transport. [`DeepSeekHarness`] /
//! [`HarnessSession`] own one `session/prompt` through its inbox receipt and
//! the next root `idle`, merging descendant notifications from
//! `subagent.started`.

mod api;
mod error;
mod notification;
mod run;

pub use api::{
    mint_session_id, resolve_workspace_cwd, DeepSeekHarness, DeepSeekHarnessOptions,
    HarnessClientOptions, HarnessSession, RunOptions, DEFAULT_MODEL, DEFAULT_PROVIDER,
};
pub use error::{SdkError, SdkProtocolError};
pub use notification::{
    in_session_tree, is_descendant_of, is_inbox_receipt, is_record, record_session_relationship,
    validated_session_event, HarnessNotification, SessionParents,
};
pub use run::{
    collect_run, collect_run_with, final_assistant_text, final_response, normalize_input,
    RunCollector, RunInput, RunResult,
};

use dsh_sdk_protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
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
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        Self::spawn_with(program, &owned, None, None).await
    }

    /// Spawn with an optional child cwd and a replacement environment.
    ///
    /// `env: None` inherits the parent environment. `env: Some` replaces it.
    pub async fn spawn_with(
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: Option<&HashMap<String, String>>,
    ) -> std::io::Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = env {
            command.env_clear().envs(env);
        }
        let mut child = command.spawn()?;
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
        max_tokens: Option<i64>,
    ) -> std::io::Result<Value> {
        let mut params = serde_json::json!({
            "cwd": cwd,
            "provider": provider,
            "model": model,
        });
        if let Some(max_tokens) = max_tokens {
            params["maxTokens"] = serde_json::json!(max_tokens);
        }
        self.call(methods::INITIALIZE, Some(params)).await
    }

    /// `session/prompt`: one text turn on `session_id`. Returns the enqueue
    /// receipt; notifications that arrive during the RPC land in
    /// [`Self::take_notifications`].
    pub async fn prompt(&mut self, session_id: &str, text: &str) -> std::io::Result<Value> {
        self.prompt_blocks(
            session_id,
            &[serde_json::json!({ "type": "text", "text": text })],
        )
        .await
    }

    /// `session/prompt` with caller-supplied content blocks.
    pub async fn prompt_blocks(
        &mut self,
        session_id: &str,
        content_blocks: &[Value],
    ) -> std::io::Result<Value> {
        self.call(
            methods::SESSION_PROMPT,
            Some(serde_json::json!({
                "sessionId": session_id,
                "contentBlocks": content_blocks,
            })),
        )
        .await
    }

    /// Drain the notifications received so far, in wire order.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.notifications)
    }

    /// Read the next server notification after draining any already-buffered
    /// frames. Response frames (an `id` member) are skipped.
    pub async fn next_notification(&mut self) -> std::io::Result<Value> {
        if let Some(buffered) = self.notifications.first().cloned() {
            self.notifications.remove(0);
            return Ok(buffered);
        }
        loop {
            let mut frame = String::new();
            let read = self.stdout.read_line(&mut frame).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "runtime stdout closed",
                ));
            }
            let Ok(value) = serde_json::from_str::<Value>(&frame) else {
                continue;
            };
            if value.get("id").is_none() {
                return Ok(value);
            }
        }
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
            let read = self.stdout.read_line(&mut frame).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "runtime stdout closed",
                ));
            }
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

    /// Protocol `shutdown` then kill the child runtime.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        let _ = self.call(methods::SHUTDOWN, None).await;
        let _ = self.child.kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sdk_protocol::methods;
    use std::io::Write;
    use std::process::Command as StdCommand;

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

    fn python3() -> Option<String> {
        let ok = StdCommand::new("python3")
            .arg("-c")
            .arg("import sys")
            .status()
            .ok()?
            .success();
        if ok {
            Some("python3".into())
        } else {
            None
        }
    }

    fn write_fake_runtime(path: &std::path::Path) {
        let script = r#"
import json, os, sys

marker = os.environ.get("FAKE_INIT_ERROR_ONCE_FILE")
malformed = os.environ.get("FAKE_MALFORMED")
text = os.environ.get("FAKE_TEXT", "hello from fake runtime")
subagent = os.environ.get("FAKE_SUBAGENT")

def reply(req, result=None, error=None):
    frame = {"jsonrpc": "2.0", "id": req["id"]}
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def notify(method, params):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if method == "initialize":
        if malformed:
            reply(req, result={"serverInfo": {}})
            continue
        if marker and not os.path.exists(marker):
            open(marker, "w").close()
            reply(req, error={"code": 7, "message": "scripted first-boot failure"})
            continue
        reply(req, result={
            "serverInfo": {
                "name": "deepseek-harness-sdk-runtime",
                "version": "0.0.1",
            }
        })
    elif method == "session/prompt":
        if malformed:
            reply(req, result={})
            continue
        sid = req["params"]["sessionId"]
        mid = "accepted-message"
        reply(req, result={"messageId": mid})
        notify("session.status", {"sessionId": sid, "status": "running"})
        notify("session.event", {
            "sessionId": sid,
            "event": {"type": "turn/start", "data": {"turn": 1}},
        })
        notify("session.event", {
            "sessionId": sid,
            "event": {
                "type": "agent/inbox/spliced",
                "data": {"inserted": [{"id": mid, "role": "user", "content": []}]},
            },
        })
        if subagent:
            child = sid + "-child"
            notify("subagent.started", {"parentSessionId": sid, "childSessionId": child})
            notify("session.event", {
                "sessionId": child,
                "event": {
                    "type": "assistant/message",
                    "data": {"message": {"content": [{"type": "text", "text": "child says hi"}]}},
                },
            })
            notify("subagent.finished", {"parentSessionId": sid, "childSessionId": child})
        notify("session.event", {
            "sessionId": sid,
            "event": {
                "type": "assistant/message",
                "data": {"message": {"content": [{"type": "text", "text": text}]}},
            },
        })
        notify("session.status", {"sessionId": sid, "status": "idle"})
    elif method == "shutdown":
        reply(req, result={})
"#;
        let mut file = std::fs::File::create(path).expect("fake runtime");
        file.write_all(script.as_bytes())
            .expect("write fake runtime");
    }

    #[tokio::test]
    async fn harness_run_collects_receipt_to_idle_over_stdio() {
        let Some(python) = python3() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("dsh-sdk-client-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake_runtime.py");
        write_fake_runtime(&script);
        let harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
            launch: HarnessClientOptions {
                command: python,
                args: vec![script.to_string_lossy().into_owned()],
                cwd: None,
                env: None,
            },
            cwd: Some(dir.clone()),
            provider: Some("custom-provider".into()),
            model: Some("custom-model".into()),
            max_tokens: Some(4096),
        });
        let first = harness
            .run(
                "say hi",
                RunOptions {
                    session_id: Some("owned".into()),
                    on_notification: None,
                },
            )
            .await
            .expect("run");
        assert_eq!(first.session_id, "owned");
        assert_eq!(first.final_response, "hello from fake runtime");
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["agent/inbox/spliced", "assistant/message"]
        );
        let second = harness
            .run("again", RunOptions::default())
            .await
            .expect("second run");
        assert_ne!(second.session_id, first.session_id);
        harness.close().await.expect("close");
        let after_close = harness.run("after", RunOptions::default()).await;
        assert!(matches!(after_close, Err(SdkError::Closed(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn harness_merges_descendant_notifications() {
        let Some(python) = python3() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("dsh-sdk-client-sub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake_runtime.py");
        write_fake_runtime(&script);
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("FAKE_SUBAGENT".into(), "1".into());
        env.insert("FAKE_TEXT".into(), "root says hi".into());
        let harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
            launch: HarnessClientOptions {
                command: python,
                args: vec![script.to_string_lossy().into_owned()],
                cwd: None,
                env: Some(env),
            },
            cwd: Some(dir.clone()),
            provider: None,
            model: None,
            max_tokens: None,
        });
        let mut seen = Vec::new();
        let result = harness
            .run(
                "delegate",
                RunOptions {
                    session_id: Some("parent-1".into()),
                    on_notification: Some(&mut |notification| {
                        seen.push(notification.method.clone());
                    }),
                },
            )
            .await
            .expect("run");
        assert!(seen.contains(&"subagent.started".to_string()));
        assert!(seen.contains(&"subagent.finished".to_string()));
        assert!(result.notifications.iter().any(|notification| {
            notification.method == "session.event"
                && notification.params["sessionId"] == "parent-1-child"
        }));
        assert_eq!(result.final_response, "root says hi");
        assert!(result.events.iter().all(|event| {
            event["type"] != "assistant/message"
                || event["data"]["message"]["content"][0]["text"] != "child says hi"
        }));
        harness.close().await.expect("close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_handshake_retries_on_a_fresh_client() {
        let Some(python) = python3() else {
            return;
        };
        let dir =
            std::env::temp_dir().join(format!("dsh-sdk-client-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake_runtime.py");
        write_fake_runtime(&script);
        let marker = dir.join("first-boot-failed");
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert(
            "FAKE_INIT_ERROR_ONCE_FILE".into(),
            marker.to_string_lossy().into_owned(),
        );
        env.insert("FAKE_TEXT".into(), "second boot answer".into());
        let harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
            launch: HarnessClientOptions {
                command: python,
                args: vec![script.to_string_lossy().into_owned()],
                cwd: None,
                env: Some(env),
            },
            cwd: Some(dir.clone()),
            provider: None,
            model: None,
            max_tokens: None,
        });
        let first = harness.start().await;
        assert!(first.is_err(), "first handshake should fail");
        let result = harness
            .run("again", RunOptions::default())
            .await
            .expect("retry");
        assert_eq!(result.final_response, "second boot answer");
        harness.close().await.expect("close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn malformed_initialize_is_a_protocol_error() {
        let Some(python) = python3() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("dsh-sdk-client-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake_runtime.py");
        write_fake_runtime(&script);
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("FAKE_MALFORMED".into(), "1".into());
        let harness = DeepSeekHarness::new(DeepSeekHarnessOptions {
            launch: HarnessClientOptions {
                command: python,
                args: vec![script.to_string_lossy().into_owned()],
                cwd: None,
                env: Some(env),
            },
            cwd: Some(dir.clone()),
            provider: None,
            model: None,
            max_tokens: None,
        });
        let error = harness.run("bad", RunOptions::default()).await;
        assert!(matches!(error, Err(SdkError::Protocol(_))));
        harness.close().await.expect("close");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
