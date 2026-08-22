//! Built-bin smokes: `dsh --profile acp` and `dsh --profile jsonrpc` serve
//! newline-delimited JSON-RPC over stdio with the keyless replay overlay.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

const REPLY: &str = "SDK snapshot OK";

fn spawn_profile(profile: &str) -> Child {
    let home = std::env::temp_dir().join(format!("dsh-bin-smoke-{profile}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args(["--profile", profile])
        .env("DSH_HOME", &home)
        .env("DSH_REPLAY_TEXT", REPLY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dsh")
}

fn send(child: &mut Child, frame: Value) {
    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(stdin, "{frame}").expect("write frame");
}

fn read_frame(reader: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read frame");
    serde_json::from_str(&line).expect("frame json")
}

#[test]
fn acp_profile_serves_handshake_session_and_prompt() {
    let mut child = spawn_profile("acp");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
    );
    let initialize = read_frame(&mut reader);
    assert_eq!(initialize["result"]["protocolVersion"], 1);
    assert_eq!(
        initialize["result"]["agentInfo"]["name"],
        "deepseek-harness-acp"
    );
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}),
    );
    let new_session = read_frame(&mut reader);
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Reply with exactly: SDK snapshot OK"}],
        }}),
    );
    let update = read_frame(&mut reader);
    assert_eq!(update["method"], "session/update");
    assert_eq!(
        update["params"]["update"],
        serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": REPLY},
        })
    );
    let prompt = read_frame(&mut reader);
    assert_eq!(prompt["result"]["stopReason"], "end_turn");
    drop(child.stdin.take());
    let status = child.wait().expect("dsh exit");
    assert!(status.success());
}

#[test]
fn jsonrpc_profile_streams_events_around_the_receipt() {
    let mut child = spawn_profile("jsonrpc");
    let reader = BufReader::new(child.stdout.take().expect("stdout"));
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "cwd": "/tmp",
            "provider": "deepseek-official",
            "model": "deepseek-v4-flash",
        }}),
    );
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{
            "sessionId": "11111111-1111-1111-1111-111111111111",
            "contentBlocks": [{"type":"text","text":"Reply with exactly: SDK snapshot OK"}],
        }}),
    );
    send(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"shutdown"}),
    );
    drop(child.stdin.take());
    let mut lines = Vec::new();
    for line in reader.lines() {
        lines.push(serde_json::from_str::<Value>(&line.expect("line")).expect("frame json"));
    }
    let status = child.wait().expect("dsh exit");
    assert!(status.success());
    assert_eq!(
        lines[0]["result"]["serverInfo"]["name"],
        "deepseek-harness-sdk-runtime"
    );
    assert_eq!(lines[1]["method"], "session.event");
    assert_eq!(lines[1]["params"]["event"]["type"], "agent/inbox/spliced");
    assert_eq!(lines[2]["params"]["status"], "running");
    let receipt = lines
        .iter()
        .find(|frame| frame["id"] == 2)
        .expect("prompt receipt");
    assert!(receipt["result"]["messageId"].is_string());
    let idle = lines
        .iter()
        .filter(|frame| frame["method"] == "session.status")
        .next_back()
        .expect("status frames");
    assert_eq!(idle["params"]["status"], "idle");
    assert_eq!(lines.last().unwrap()["result"], serde_json::json!({}));
    let event_types: Vec<&str> = lines
        .iter()
        .filter(|frame| frame["method"] == "session.event")
        .map(|frame| frame["params"]["event"]["type"].as_str().unwrap())
        .collect();
    assert!(event_types.contains(&"turn/start"));
    assert!(event_types.contains(&"assistant/message"));
    assert!(event_types.contains(&"turn/end"));
}
