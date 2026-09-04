//! Decode an SSE body into event `data` payloads.
//!
//! Ports `packages/llm/llm-deepseek/src/sse.ts`. Framing is spec-strict: an
//! event dispatches only on its blank-line terminator. EOF before `[DONE]`
//! is `STREAM_CLOSED`.

use dsh_llm::{LlmError, LlmFailure};

/// The terminal payload DeepSeek (and OpenAI) send after the last chunk.
pub const DONE: &str = "[DONE]";

/// Parse an SSE document into data payloads, including the `[DONE]` sentinel.
///
/// # Errors
/// `STREAM_CLOSED` when the body ends without `[DONE]`.
pub fn parse_sse(body: &str) -> Result<Vec<String>, LlmError> {
    let mut payloads = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut saw_field = false;
    for line in body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        if line.is_empty() {
            if saw_field {
                let data = data_lines.join("\n");
                payloads.push(data.clone());
                data_lines.clear();
                saw_field = false;
                if data == DONE {
                    return Ok(payloads);
                }
            }
            continue;
        }
        saw_field = true;
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
    }
    Err(LlmError::Failure(LlmFailure::new(
        "SSE stream ended without [DONE]",
        "STREAM_CLOSED",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yields_payloads_and_done() {
        let payloads = parse_sse("data: {\"a\":1}\n\ndata: [DONE]\n\n").unwrap();
        assert_eq!(payloads, vec![r#"{"a":1}"#, DONE]);
    }

    #[test]
    fn refuses_truncation() {
        let err = parse_sse("data: {\"a\":1}\n\n").unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "STREAM_CLOSED");
        assert_eq!(failure.message, "SSE stream ended without [DONE]");
    }
}
