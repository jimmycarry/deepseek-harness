//! Translate DeepSeek SSE payloads into harness `StreamChunk`s.
//!
//! Ports `packages/llm/llm-deepseek/src/translate.ts`. Finish reason and the
//! latest usage are deferred until `[DONE]`. A `stop` finish with no opened
//! blocks is `EMPTY_RESPONSE`.

use dsh_llm::{
    call_id, ContentBlock, FinishReason, LlmError, LlmFailure, StreamChunk, TokenUsage,
    EMPTY_RESPONSE_CODE,
};
use serde_json::Value;

use crate::sse::DONE;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Reasoning,
    ToolCall,
}

struct OpenBlock {
    index: u32,
    kind: BlockKind,
    text: String,
    call_id: Option<String>,
    name: Option<String>,
}

/// Map the wire `finish_reason` vocabulary to the harness finish reason.
pub fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::MaxTokens,
        other => FinishReason::Error {
            failure: LlmFailure::new(
                format!("model stopped: {other}"),
                other.to_ascii_uppercase(),
            ),
        },
    }
}

/// Map wire usage fields. DeepSeek `prompt_tokens` includes cache hits;
/// harness `input_tokens` is the uncached remainder.
pub fn map_usage(usage: &Value) -> TokenUsage {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let cache_read = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.get("prompt_cache_hit_tokens"))
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    TokenUsage {
        input_tokens: prompt.saturating_sub(cache_read.unwrap_or(0)),
        output_tokens: completion,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
    }
}

fn close_block(block: &OpenBlock) -> ContentBlock {
    match block.kind {
        BlockKind::Text => ContentBlock::text(block.text.clone()),
        BlockKind::Reasoning => ContentBlock::Reasoning {
            text: block.text.clone(),
        },
        BlockKind::ToolCall => ContentBlock::ToolCall {
            id: call_id(block.call_id.clone().unwrap_or_default()),
            name: block.name.clone().unwrap_or_default(),
            arguments: block.text.clone(),
        },
    }
}

/// Consume SSE data payloads (ending with `[DONE]`) and return StreamChunks.
///
/// # Errors
/// `MALFORMED_RESPONSE` on invalid JSON; `STREAM_CLOSED` if `[DONE]` is missing.
pub fn translate(payloads: &[String]) -> Result<Vec<StreamChunk>, LlmError> {
    let mut next_index = 0u32;
    let mut text_block: Option<usize> = None;
    let mut reasoning_block: Option<usize> = None;
    let mut tool_blocks: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    let mut order: Vec<OpenBlock> = Vec::new();
    let mut pending_finish: Option<FinishReason> = None;
    let mut pending_usage: Option<TokenUsage> = None;
    let mut chunks = Vec::new();

    let mut open = |kind: BlockKind, order: &mut Vec<OpenBlock>| -> usize {
        let index = next_index;
        next_index += 1;
        order.push(OpenBlock {
            index,
            kind,
            text: String::new(),
            call_id: None,
            name: None,
        });
        order.len() - 1
    };

    for payload in payloads {
        if payload == DONE {
            for block in &order {
                chunks.push(StreamChunk::BlockEnd {
                    index: block.index,
                    block: close_block(block),
                });
            }
            if let Some(usage) = pending_usage {
                chunks.push(StreamChunk::Usage { usage });
            }
            let reason = pending_finish.unwrap_or(FinishReason::Stop);
            let reason = if matches!(reason, FinishReason::Stop) && order.is_empty() {
                FinishReason::Error {
                    failure: LlmFailure::new(
                        "model returned a completed response with no content",
                        EMPTY_RESPONSE_CODE,
                    ),
                }
            } else {
                reason
            };
            chunks.push(StreamChunk::Finish {
                reason,
                replay_state: None,
            });
            return Ok(chunks);
        }

        let chunk: Value = serde_json::from_str(payload).map_err(|_| {
            LlmError::Failure(LlmFailure::new(
                format!(
                    "malformed SSE payload: {}",
                    payload.chars().take(120).collect::<String>()
                ),
                "MALFORMED_RESPONSE",
            ))
        })?;

        if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let delta = choice.get("delta");
                let reasoning = delta
                    .and_then(|delta| delta.get("reasoning_content"))
                    .and_then(Value::as_str);
                if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
                    if reasoning_block.is_none() {
                        let slot = open(BlockKind::Reasoning, &mut order);
                        reasoning_block = Some(slot);
                        chunks.push(StreamChunk::BlockStart {
                            index: order[slot].index,
                            block_type: "reasoning".into(),
                        });
                    }
                    let slot = reasoning_block.expect("opened");
                    order[slot].text.push_str(reasoning);
                    chunks.push(StreamChunk::ReasoningDelta {
                        index: order[slot].index,
                        text: reasoning.to_string(),
                    });
                }

                let content = delta
                    .and_then(|delta| delta.get("content"))
                    .and_then(Value::as_str);
                if let Some(content) = content.filter(|text| !text.is_empty()) {
                    if text_block.is_none() {
                        let slot = open(BlockKind::Text, &mut order);
                        text_block = Some(slot);
                        chunks.push(StreamChunk::BlockStart {
                            index: order[slot].index,
                            block_type: "text".into(),
                        });
                    }
                    let slot = text_block.expect("opened");
                    order[slot].text.push_str(content);
                    chunks.push(StreamChunk::TextDelta {
                        index: order[slot].index,
                        text: content.to_string(),
                    });
                }

                if let Some(calls) = delta
                    .and_then(|delta| delta.get("tool_calls"))
                    .and_then(Value::as_array)
                {
                    for call in calls {
                        let wire_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let slot = if let Some(slot) = tool_blocks.get(&wire_index) {
                            *slot
                        } else {
                            let slot = open(BlockKind::ToolCall, &mut order);
                            tool_blocks.insert(wire_index, slot);
                            chunks.push(StreamChunk::BlockStart {
                                index: order[slot].index,
                                block_type: "tool-call".into(),
                            });
                            slot
                        };
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            order[slot].call_id = Some(id.to_string());
                        }
                        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                            order[slot].name = Some(name.to_string());
                        }
                        let fragment = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        order[slot].text.push_str(fragment);
                        chunks.push(StreamChunk::ToolCallDelta {
                            index: order[slot].index,
                            id: call_id(order[slot].call_id.clone().unwrap_or_default()),
                            name: order[slot].name.clone(),
                            arguments_delta: fragment.to_string(),
                        });
                    }
                }

                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    pending_finish = Some(map_finish_reason(reason));
                }
            }
        }
        if chunk.get("usage").is_some() {
            pending_usage = Some(map_usage(chunk.get("usage").expect("checked")));
        }
    }

    Err(LlmError::Failure(LlmFailure::new(
        "SSE payload stream ended without [DONE]",
        "STREAM_CLOSED",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::DONE;

    #[test]
    fn translates_text_and_defers_finish() {
        let chunks = translate(&[
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#.into(),
            DONE.into(),
        ])
        .unwrap();
        assert!(matches!(
            &chunks[0],
            StreamChunk::BlockStart { block_type, .. } if block_type == "text"
        ));
        assert!(matches!(
            chunks.last(),
            Some(StreamChunk::Finish {
                reason: FinishReason::Stop,
                ..
            })
        ));
    }

    #[test]
    fn empty_stop_is_empty_response() {
        let chunks = translate(&[
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.into(),
            DONE.into(),
        ])
        .unwrap();
        match chunks.last() {
            Some(StreamChunk::Finish {
                reason: FinishReason::Error { failure },
                ..
            }) => {
                assert_eq!(failure.code, EMPTY_RESPONSE_CODE);
                assert_eq!(
                    failure.message,
                    "model returned a completed response with no content"
                );
            }
            other => panic!("expected empty-response finish, got {other:?}"),
        }
    }

    #[test]
    fn maps_length_to_max_tokens() {
        assert!(matches!(
            map_finish_reason("length"),
            FinishReason::MaxTokens
        ));
    }
}
