//! First-human-message model provider for `ctx.sessionTitle`. The exact
//! model-visible request is appended as `session/title-llm-request` before
//! dispatch; the auxiliary call runs with `purpose: "session-title"` and a
//! failure keeps the standing title.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_llm::{
    BlockAssembler, ContentBlock, FinishReason, LlmCallConfig, LlmRequest, LlmRuntime, Message,
    MessageSource, StreamChunk, UserMessage,
};
use dsh_session::{Session, SessionEventData};
use dsh_session_title::{
    normalize_session_title, SessionTitleProvider, SessionTitleResult, SessionTitleRoute,
    SessionTitleService, SessionTitleUserMessage,
};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// Stable provider id recorded with generated titles and logged requests.
pub const PROVIDER_ID: &str = "session-title-first-prompt-llm";

/// Required deployment policy; this plugin adds no defaults.
#[derive(Debug, Clone)]
pub struct SessionTitleLlmConfig {
    /// Target word count for non-CJK titles.
    pub target_words: u32,
    /// Target character count for Chinese, Japanese, or Korean titles.
    pub target_cjk_characters: u32,
    /// Maximum UTF-8 bytes in the final JSON-framed user prompt.
    pub max_input_bytes: usize,
    /// Auxiliary generation output-token cap.
    pub max_output_tokens: u32,
    /// End-to-end auxiliary request deadline in milliseconds.
    pub timeout_ms: u64,
    /// Optional explicit provider route; must be paired with `model`.
    pub provider: Option<String>,
    /// Optional explicit model id; must be paired with `provider`.
    pub model: Option<String>,
}

impl SessionTitleLlmConfig {
    /// Validate positive limits and the paired optional route.
    pub fn validate(&self) -> Result<(), String> {
        if self.target_words == 0
            || self.target_cjk_characters == 0
            || self.max_input_bytes == 0
            || self.max_output_tokens == 0
            || self.timeout_ms == 0
        {
            return Err("session-title-llm: limits must be positive integers".into());
        }
        if self.provider.is_some() != self.model.is_some() {
            return Err("session-title-llm: provider and model must be supplied together".into());
        }
        Ok(())
    }
}

/// The registered first-prompt provider.
pub struct FirstPromptLlmProvider {
    config: SessionTitleLlmConfig,
}

/// Stable language-aware system instruction shared by model-backed title providers.
fn system_prompt(config: &SessionTitleLlmConfig) -> String {
    [
        "Create a concise title for an AI coding-assistant session from the supplied human messages.".to_string(),
        "Return only the title on one line, **in plain text of natural language**, with no quotes, prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.".to_string(),
        "Use the language of the messages.".to_string(),
        format!(
            "Aim for about {} words in non-CJK languages or {} CJK characters.",
            config.target_words, config.target_cjk_characters
        ),
    ]
    .join("\n")
}

/// Frame exact messages as JSON so user text cannot break structural delimiters.
fn frame_messages(messages: &[SessionTitleUserMessage]) -> String {
    let array: Vec<serde_json::Value> = messages
        .iter()
        .map(|message| serde_json::json!({ "seq": message.seq, "text": message.text }))
        .collect();
    format!(
        "Generate the session title from this JSON array of human messages:\n{}",
        serde_json::to_string(&array).unwrap_or_else(|_| "[]".into())
    )
}

/// Translate terminal finish reasons into an auxiliary-call failure.
fn finish_error(finish: &FinishReason) -> Option<String> {
    match finish {
        FinishReason::Stop => None,
        FinishReason::Error { failure } | FinishReason::Aborted { failure } => {
            Some(failure.message.clone())
        }
        FinishReason::MaxTokens => {
            Some("session-title-llm: title output reached maxOutputTokens".into())
        }
        FinishReason::ToolCalls => {
            Some("session-title-llm: title model unexpectedly requested a tool".into())
        }
    }
}

#[async_trait]
impl SessionTitleProvider for FirstPromptLlmProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn automatic(&self) -> &str {
        "first-prompt"
    }

    async fn generate(
        &self,
        ctx: &Context,
        session: &Session,
        messages: &[SessionTitleUserMessage],
        route: &SessionTitleRoute,
    ) -> Result<SessionTitleResult, String> {
        let Some(first) = messages.first() else {
            return Err("first-prompt title provider requires one human message".into());
        };
        let selected = vec![first.clone()];
        let framed = frame_messages(&selected);
        if framed.len() > self.config.max_input_bytes {
            return Err(format!(
                "session-title-llm: input is {} bytes, exceeding maxInputBytes {}",
                framed.len(),
                self.config.max_input_bytes
            ));
        }
        let route = match (&self.config.provider, &self.config.model) {
            (Some(provider), Some(model)) => SessionTitleRoute {
                provider: provider.clone(),
                model: model.clone(),
            },
            _ => route.clone(),
        };
        let request_messages = vec![Message::User(UserMessage::from_parts(
            vec![ContentBlock::text(framed)],
            MessageSource::plugin("dsh-session-title-llm"),
        ))];
        let system = system_prompt(&self.config);
        session
            .append(
                SessionEventData::SessionTitleLlmRequest {
                    title_provider: PROVIDER_ID.into(),
                    message_seqs: selected.iter().map(|message| message.seq).collect(),
                    route: serde_json::json!({
                        "provider": route.provider,
                        "model": route.model,
                    }),
                    system: system.clone(),
                    messages: request_messages.clone(),
                    max_tokens: self.config.max_output_tokens,
                },
                None,
            )
            .map_err(|error| error.to_string())?;
        let llm = ctx
            .service::<LlmRuntime>()
            .map_err(|error| error.to_string())?;
        let request = LlmRequest {
            adapter_defaults: None,
            config: LlmCallConfig {
                provider: route.provider.clone(),
                model: route.model.clone(),
                reasoning_effort: None,
                max_tokens: Some(self.config.max_output_tokens),
            },
            system: Some(system),
            messages: request_messages,
            tools: vec![],
            purpose: Some("session-title".into()),
        };
        let deadline = Duration::from_millis(self.config.timeout_ms);
        let stream = tokio::time::timeout(deadline, llm.stream(request))
            .await
            .map_err(|_| "session-title-llm: title request timed out".to_string())?
            .map_err(|error| error.to_string())?;
        let mut assembler = BlockAssembler::default();
        let mut finish: Option<FinishReason> = None;
        futures::pin_mut!(stream);
        let collect = async {
            while let Some(chunk) = stream.next().await {
                if let StreamChunk::Finish { reason, .. } = &chunk {
                    finish = Some(reason.clone());
                }
                assembler.push(&chunk);
            }
        };
        tokio::time::timeout(deadline, collect)
            .await
            .map_err(|_| "session-title-llm: title stream timed out".to_string())?;
        if let Some(reason) = finish.as_ref().and_then(finish_error) {
            return Err(reason);
        }
        let blocks = assembler.finish();
        if blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
        {
            return Err("session-title-llm: title output must contain text only".into());
        }
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let title = normalize_session_title(&text, usize::MAX);
        if title.is_empty() {
            return Err("session-title-llm: title model produced no text".into());
        }
        Ok(SessionTitleResult {
            title,
            message_seqs: selected.iter().map(|message| message.seq).collect(),
            model: Some(route),
        })
    }
}

/// Register the first-prompt provider with `ctx.sessionTitle`.
///
/// # Errors
/// Invalid configuration, a missing title service, or a duplicate provider.
pub fn install(ctx: &Context, config: SessionTitleLlmConfig) -> dsh_cordis::Result<()> {
    config
        .validate()
        .map_err(dsh_cordis::CordisError::Plugin)?;
    let titles = ctx.service::<SessionTitleService>()?;
    titles
        .register(Arc::new(FirstPromptLlmProvider { config }))
        .map_err(dsh_cordis::CordisError::Plugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_paired_route_and_positive_limits() {
        let base = SessionTitleLlmConfig {
            target_words: 5,
            target_cjk_characters: 10,
            max_input_bytes: 4096,
            max_output_tokens: 64,
            timeout_ms: 60_000,
            provider: None,
            model: None,
        };
        assert!(base.validate().is_ok());
        let mut unpaired = base.clone();
        unpaired.provider = Some("deepseek".into());
        assert!(unpaired.validate().is_err());
        let mut zero = base;
        zero.max_output_tokens = 0;
        assert!(zero.validate().is_err());
    }

    #[test]
    fn framing_and_system_prompt_match_typescript_text() {
        let framed = frame_messages(&[SessionTitleUserMessage {
            seq: 7,
            text: "Prove the product headless profile path with one real tool round trip.".into(),
        }]);
        assert_eq!(
            framed,
            "Generate the session title from this JSON array of human messages:\n[{\"seq\":7,\"text\":\"Prove the product headless profile path with one real tool round trip.\"}]"
        );
        let system = system_prompt(&SessionTitleLlmConfig {
            target_words: 5,
            target_cjk_characters: 10,
            max_input_bytes: 4096,
            max_output_tokens: 64,
            timeout_ms: 60_000,
            provider: None,
            model: None,
        });
        assert!(system.starts_with(
            "Create a concise title for an AI coding-assistant session from the supplied human messages."
        ));
        assert!(system.ends_with("Aim for about 5 words in non-CJK languages or 10 CJK characters."));
    }
}
