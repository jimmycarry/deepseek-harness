//! Spill-policy plugin: a `tools/post-execute` transformer.
//!
//! When a final plain-text result's UTF-8 size exceeds `max_inline_bytes`,
//! it saves the full text through `ctx.spillStore` and replaces the
//! model-facing result with a head/tail preview plus the locator. Omitted
//! `max_inline_bytes` registers nothing. A spill failure keeps the inline
//! result and never turns a success into `isError`. `read` is skipped.

use dsh_cordis::{Context, Result};
use dsh_spill::{SaveTextSpill, SpillOwner, SpillRef, SpillSource, SpillStore};
use serde_json::Value;

/// Plugin construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Model-facing UTF-8 byte cap. `None` disables the policy.
    pub max_inline_bytes: Option<usize>,
}

impl Config {
    /// Resolve plugin config. A present non-integer or negative cap fails load.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let Some(raw) = value.and_then(|value| value.get("maxInlineBytes")) else {
            return Ok(Self {
                max_inline_bytes: None,
            });
        };
        let Some(number) = raw.as_u64() else {
            return Err(format!(
                "spill-policy: maxInlineBytes must be a non-negative integer (got {raw})"
            ));
        };
        Ok(Self {
            max_inline_bytes: Some(usize::try_from(number).map_err(|_| {
                format!("spill-policy: maxInlineBytes must be a non-negative integer (got {raw})")
            })?),
        })
    }
}

/// Register the post-execute arm when a cap is configured.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let Some(cap) = config.max_inline_bytes else {
        return Ok(());
    };
    let ctx_for_store = ctx.clone();
    ctx.on_waterfall("tools/post-execute", move |payload, next| {
        let mut downstream = next.call(payload);
        let Some(map) = downstream.as_object_mut() else {
            return downstream;
        };
        let name = map
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name == "read" {
            return downstream;
        }
        let agent_id = map
            .get("agentId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let content = map.get("content").cloned().unwrap_or(Value::Null);
        let Some(text) = flatten_plain_text(&content) else {
            return downstream;
        };
        let total_bytes = text.len();
        if total_bytes <= cap {
            return downstream;
        }
        let Some(replaced) = spill_replacement(
            &ctx_for_store,
            &text,
            total_bytes,
            agent_id.as_deref(),
            &name,
            cap,
        ) else {
            return downstream;
        };
        map.insert(
            "content".into(),
            serde_json::json!([{ "type": "text", "text": replaced }]),
        );
        downstream
    })
    .map(|_| ())
}

fn flatten_plain_text(content: &Value) -> Option<String> {
    let blocks = content.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        text.push_str(block.get("text").and_then(Value::as_str)?);
    }
    Some(text)
}

fn spill_replacement(
    ctx: &Context,
    text: &str,
    total_bytes: usize,
    session_id: Option<&str>,
    tool_name: &str,
    cap: usize,
) -> Option<String> {
    let session_id = session_id?;
    let store = ctx.get::<SpillStore>()?;
    let save = SaveTextSpill {
        owner: SpillOwner {
            session_id: session_id.to_string(),
        },
        source: SpillSource {
            tool_name: tool_name.to_string(),
            call_id: String::new(),
            label: "result".into(),
        },
        suggested_name: format!("{tool_name}.txt"),
        content: text.to_string(),
    };
    let ref_ = store.save_text(save).ok()?;
    let reserve = spill_notice(
        &Omitted {
            kind: OmittedKind::Exact,
            count: total_bytes,
        },
        &ref_,
    )
    .len()
        + 2;
    let preview_budget = cap.saturating_sub(reserve);
    let (preview_text, omitted) = preview(text, preview_budget);
    let notice = spill_notice(&omitted, &ref_);
    let replaced = if preview_text.is_empty() {
        notice
    } else {
        format!("{preview_text}\n\n{notice}")
    };
    if replaced.len() > cap {
        return None;
    }
    Some(replaced)
}

#[derive(Debug, Clone, Copy)]
enum OmittedKind {
    None,
    Exact,
}

struct Omitted {
    kind: OmittedKind,
    count: usize,
}

fn describe_omitted(omitted: &Omitted) -> String {
    match omitted.kind {
        OmittedKind::None => String::new(),
        OmittedKind::Exact => format!("Omitted {} bytes.", omitted.count),
    }
}

fn spill_notice(omitted: &Omitted, reference: &SpillRef) -> String {
    let omission = describe_omitted(omitted);
    let prefix = if omission.is_empty() {
        String::new()
    } else {
        format!("{omission} ")
    };
    format!(
        "({prefix}Full formatted result stored at: {}. {})",
        reference.locator.0, reference.retrieval_hint
    )
}

fn preview(text: &str, budget: usize) -> (String, Omitted) {
    if text.len() <= budget {
        return (
            text.to_string(),
            Omitted {
                kind: OmittedKind::None,
                count: 0,
            },
        );
    }
    let head = budget.div_ceil(2);
    let tail = budget / 2;
    let head_end = floor_char_boundary(text, head);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail));
    if tail_start < head_end {
        return (
            text.to_string(),
            Omitted {
                kind: OmittedKind::None,
                count: 0,
            },
        );
    }
    let omitted = text.len() - head_end - (text.len() - tail_start);
    let mut kept = String::new();
    kept.push_str(&text[..head_end]);
    kept.push_str(&text[tail_start..]);
    (
        kept,
        Omitted {
            kind: OmittedKind::Exact,
            count: omitted,
        },
    )
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn ceil_char_boundary(text: &str, min_bytes: usize) -> usize {
    let mut start = min_bytes.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::ContentBlock;
    use dsh_spill::{SpillBackend, SpillError};
    use dsh_tools::{ScriptTool, ToolOutcome, ToolRuntime};
    use std::sync::{Arc, Mutex};

    struct RecordingSpill {
        saves: Mutex<Vec<String>>,
    }

    impl SpillBackend for RecordingSpill {
        fn save_text(
            &self,
            input: SaveTextSpill,
        ) -> std::result::Result<SpillRef, SpillError> {
            self.saves.lock().expect("saves").push(input.content.clone());
            Ok(SpillRef {
                locator: dsh_spill::SpillLocator("/spill/bash.txt".into()),
                bytes: input.content.len(),
                retrieval_hint: "Use the stub retrieval path.".into(),
            })
        }
    }

    #[tokio::test]
    async fn omitted_cap_registers_nothing() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("echo", "echo", |_| {
            ToolOutcome::text("x".repeat(80))
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        install(&ctx, Config { max_inline_bytes: None }).unwrap();
        let result = tools
            .execute_for(&ctx, "echo", serde_json::json!({}), Some("s"))
            .await
            .unwrap();
        assert_eq!(result.outcome.content[0], ContentBlock::text("x".repeat(80)));
    }

    #[tokio::test]
    async fn oversized_plain_text_is_replaced() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        let body = "abcdefghij".repeat(50);
        let expected = body.clone();
        tools.insert(Arc::new(ScriptTool::new("echo", "echo", move |_| {
            ToolOutcome::text(expected.clone())
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        ctx.provide(Arc::new(SpillStore::new(Arc::new(RecordingSpill {
            saves: Mutex::new(Vec::new()),
        }))))
        .unwrap();
        install(
            &ctx,
            Config {
                max_inline_bytes: Some(200),
            },
        )
        .unwrap();
        let result = tools
            .execute_for(&ctx, "echo", serde_json::json!({}), Some("s1"))
            .await
            .unwrap();
        let text = match &result.outcome.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("text"),
        };
        assert!(text.contains("Full formatted result stored at: /spill/bash.txt"), "{text}");
        assert!(text.contains("Omitted"), "{text}");
        assert!(text.len() <= 200, "{}", text.len());
        assert!(!result.outcome.is_error);
    }

    #[tokio::test]
    async fn read_is_not_spilled() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("read", "read", |_| {
            ToolOutcome::text("y".repeat(200))
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        ctx.provide(Arc::new(SpillStore::new(Arc::new(RecordingSpill {
            saves: Mutex::new(Vec::new()),
        }))))
        .unwrap();
        install(
            &ctx,
            Config {
                max_inline_bytes: Some(40),
            },
        )
        .unwrap();
        let result = tools
            .execute_for(&ctx, "read", serde_json::json!({}), Some("s1"))
            .await
            .unwrap();
        assert_eq!(result.outcome.content[0], ContentBlock::text("y".repeat(200)));
    }

    #[test]
    fn resolve_rejects_negative() {
        let err = Config::resolve(Some(&serde_json::json!({ "maxInlineBytes": -1 }))).unwrap_err();
        assert!(err.contains("non-negative integer"));
    }
}
