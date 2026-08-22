//! Out-of-process JSON-RPC client. Projects the loop; does not reimplement it.

use dsh_sdk_protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Stdio JSON-RPC client over a spawned runtime.
pub struct JsonRpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
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
        })
    }

    /// `initialize`.
    pub async fn initialize(&mut self) -> std::io::Result<Value> {
        self.call(methods::INITIALIZE, None).await
    }

    /// `session/prompt`.
    pub async fn prompt(&mut self, text: &str) -> std::io::Result<Value> {
        self.call(methods::SESSION_PROMPT, Some(serde_json::json!({ "text": text })))
            .await
    }

    async fn call(&mut self, method: &str, params: Option<Value>) -> std::io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = JsonRpcRequest::new(id, method, params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        let mut response = String::new();
        self.stdout.read_line(&mut response).await?;
        let parsed: JsonRpcResponse = serde_json::from_str(&response)?;
        Ok(parsed.result.unwrap_or(Value::Null))
    }

    /// Kill the child runtime.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        let _ = self.call(methods::SHUTDOWN, None).await;
        let _ = self.child.kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dsh_sdk_protocol::methods;

    #[test]
    fn projects_the_same_method_names() {
        assert_eq!(methods::SESSION_PROMPT, "session/prompt");
    }
}
