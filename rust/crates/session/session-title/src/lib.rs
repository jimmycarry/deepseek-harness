//! Log-backed session titles (`ctx.sessionTitle`): a deterministic fallback
//! derived from the first human message plus an optional model-backed
//! provider whose request is logged as `session/title-llm-request`.

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use dsh_llm::{ContentBlock, MessageSource};
use dsh_session::{Session, SessionEventData};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Deployment title budgets, validated at construction.
#[derive(Debug, Clone)]
pub struct SessionTitleConfig {
    /// Word cap for the deterministic fallback.
    pub fallback_max_words: usize,
    /// UTF-8 byte cap for the deterministic fallback.
    pub fallback_max_bytes: usize,
    /// UTF-8 byte cap for any accepted title.
    pub max_title_bytes: usize,
}

impl SessionTitleConfig {
    /// Validate that every budget is a positive integer.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("fallbackMaxWords", self.fallback_max_words),
            ("fallbackMaxBytes", self.fallback_max_bytes),
            ("maxTitleBytes", self.max_title_bytes),
        ] {
            if value == 0 {
                return Err(format!("session-title: {name} must be a positive integer"));
            }
        }
        Ok(())
    }
}

/// One eligible human message with its exact log seq.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTitleUserMessage {
    /// Log seq of the `user/message` event.
    pub seq: u64,
    /// Concatenated text content.
    pub text: String,
}

/// Exact auxiliary model route used for one title.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTitleRoute {
    /// Provider route.
    pub provider: String,
    /// Model id.
    pub model: String,
}

/// Provider-produced title revision.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTitleResult {
    /// Proposed title before service normalization.
    pub title: String,
    /// Exact source-message seqs.
    pub message_seqs: Vec<u64>,
    /// Route that produced the title, when a model produced it.
    pub model: Option<SessionTitleRoute>,
}

/// One optional asynchronous title implementation registered with the service.
#[async_trait]
pub trait SessionTitleProvider: Send + Sync {
    /// Stable provider id recorded with generated titles.
    fn id(&self) -> &str;
    /// Automatic cadence: `first-prompt` or `all-prompts`.
    fn automatic(&self) -> &str;
    /// Produce one title revision; the implementation logs its own
    /// `session/title-llm-request` before dispatch.
    ///
    /// # Errors
    /// A failed auxiliary call; the current title stands.
    async fn generate(
        &self,
        ctx: &Context,
        session: &Session,
        messages: &[SessionTitleUserMessage],
        route: &SessionTitleRoute,
    ) -> Result<SessionTitleResult, String>;
}

/// `ctx.sessionTitle`.
pub struct SessionTitleService {
    config: SessionTitleConfig,
    provider: Mutex<Option<Arc<dyn SessionTitleProvider>>>,
    attempted: Mutex<HashSet<String>>,
}

impl Service for SessionTitleService {
    const KEY: &'static str = "sessionTitle";
}

impl SessionTitleService {
    /// Build from validated budgets.
    pub fn new(config: SessionTitleConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            provider: Mutex::new(None),
            attempted: Mutex::new(HashSet::new()),
        })
    }

    /// Provide `ctx.sessionTitle`.
    ///
    /// # Errors
    /// Invalid budgets or a duplicate service registration.
    pub fn install(ctx: &Context, config: SessionTitleConfig) -> dsh_cordis::Result<Arc<Self>> {
        let service =
            Arc::new(Self::new(config).map_err(dsh_cordis::CordisError::Plugin)?);
        ctx.provide(Arc::clone(&service))?;
        Ok(service)
    }

    /// Register the single optional title provider; a second registration fails loud.
    pub fn register(&self, provider: Arc<dyn SessionTitleProvider>) -> Result<(), String> {
        let mut slot = self.provider.lock().expect("provider");
        if slot.is_some() {
            return Err("session-title: a title provider is already registered".into());
        }
        *slot = Some(provider);
        Ok(())
    }

    /// Append the deterministic fallback title unless the session already has one.
    /// `seq` and `text` identify the first human message of the step.
    pub fn ensure_fallback(&self, session: &Session, seq: u64, text: &str) {
        if has_title(session) {
            return;
        }
        let title = fallback_session_title(
            text,
            self.config.fallback_max_words,
            self.config.fallback_max_bytes,
        );
        if title.is_empty() {
            return;
        }
        session
            .append(
                SessionEventData::SessionTitle {
                    title,
                    message_seqs: vec![seq],
                    source: serde_json::json!({ "kind": "fallback" }),
                },
                None,
            )
            .ok();
    }

    /// Run the first-prompt provider once per top-level session after the
    /// exact main-request route is logged. A provider failure keeps the
    /// standing title.
    pub async fn on_request_logged(
        &self,
        ctx: &Context,
        session: &Session,
        provider: &str,
        model: &str,
    ) {
        let registration = { self.provider.lock().expect("provider").clone() };
        let Some(registration) = registration else {
            return;
        };
        if registration.automatic() != "first-prompt" {
            return;
        }
        if session.header().parent_session.is_some() {
            return;
        }
        let messages = collect_session_title_messages(session);
        if messages.len() != 1 {
            return;
        }
        {
            let mut attempted = self.attempted.lock().expect("attempted");
            if !attempted.insert(session.id().as_str().to_string()) {
                return;
            }
        }
        let route = SessionTitleRoute {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        match registration.generate(ctx, session, &messages, &route).await {
            Ok(result) => {
                let title = normalize_session_title(&result.title, self.config.max_title_bytes);
                if title.is_empty() {
                    return;
                }
                let model = result.model.as_ref().map(|route| {
                    serde_json::json!({ "provider": route.provider, "model": route.model })
                });
                let mut source = serde_json::json!({
                    "kind": "provider",
                    "provider": registration.id(),
                });
                if let (Some(model), Some(map)) = (model, source.as_object_mut()) {
                    map.insert("model".into(), model);
                }
                session
                    .append(
                        SessionEventData::SessionTitle {
                            title,
                            message_seqs: result.message_seqs,
                            source,
                        },
                        None,
                    )
                    .ok();
            }
            Err(_error) => {}
        }
    }
}

/// Whether the log already carries a `session/title` event.
fn has_title(session: &Session) -> bool {
    session
        .events()
        .iter()
        .any(|event| matches!(event.data, SessionEventData::SessionTitle { .. }))
}

/// Collect human text-bearing user messages in log order with exact seqs.
pub fn collect_session_title_messages(session: &Session) -> Vec<SessionTitleUserMessage> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::UserMessage(message)
                if matches!(message.source, MessageSource::User) =>
            {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.trim().is_empty() {
                    None
                } else {
                    Some(SessionTitleUserMessage {
                        seq: event.seq,
                        text,
                    })
                }
            }
            _ => None,
        })
        .collect()
}

/// Remove terminal escapes and controls and produce one trimmed line.
fn clean_title_text(input: &str) -> String {
    let stripped = strip_escapes(input);
    let mut output = String::new();
    let mut pending_space = false;
    for character in stripped.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if is_removed_control(character) {
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(character);
    }
    output
}

/// Remove OSC, CSI, and two-byte ESC sequences, including unterminated tails.
fn strip_escapes(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        let osc = character == '\u{9D}'
            || (character == '\u{1B}' && chars.peek() == Some(&']'));
        if osc {
            if character == '\u{1B}' {
                chars.next();
            }
            let mut previous: Option<char> = None;
            for terminator in chars.by_ref() {
                if terminator == '\u{7}' {
                    break;
                }
                if previous == Some('\u{1B}') && terminator == '\\' {
                    break;
                }
                previous = Some(terminator);
            }
            continue;
        }
        let csi = character == '\u{9B}'
            || (character == '\u{1B}' && chars.peek() == Some(&'['));
        if csi {
            if character == '\u{1B}' {
                chars.next();
            }
            for terminator in chars.by_ref() {
                if ('\u{40}'..='\u{7E}').contains(&terminator) {
                    break;
                }
            }
            continue;
        }
        if character == '\u{1B}' {
            if let Some(next) = chars.peek() {
                if ('\u{40}'..='\u{5F}').contains(next) {
                    chars.next();
                    continue;
                }
            }
            continue;
        }
        output.push(character);
    }
    output
}

/// Non-whitespace C0/C1 controls plus directional and invisible characters.
fn is_removed_control(character: char) -> bool {
    matches!(character,
        '\u{0}'..='\u{8}'
        | '\u{B}'
        | '\u{C}'
        | '\u{E}'..='\u{1F}'
        | '\u{7F}'..='\u{9F}'
        | '\u{200B}'
        | '\u{200E}'
        | '\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}')
}

/// Truncate to a UTF-8 byte budget without splitting a code point.
///
/// # Panics
/// `max_bytes` of zero.
pub fn truncate_title_utf8(input: &str, max_bytes: usize) -> String {
    assert!(max_bytes > 0, "maxBytes must be a positive integer");
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut used = 0;
    let mut output = String::new();
    for character in input.chars() {
        let bytes = character.len_utf8();
        if used + bytes > max_bytes {
            break;
        }
        output.push(character);
        used += bytes;
    }
    output
}

/// Normalize one accepted title and enforce its UTF-8 byte budget.
pub fn normalize_session_title(input: &str, max_bytes: usize) -> String {
    truncate_title_utf8(&clean_title_text(input), max_bytes)
        .trim_end()
        .to_string()
}

/// Derive the deterministic first-prompt fallback within word and byte caps.
pub fn fallback_session_title(input: &str, max_words: usize, max_bytes: usize) -> String {
    assert!(max_words > 0, "maxWords must be a positive integer");
    let cleaned = clean_title_text(input);
    let joined = cleaned
        .split(' ')
        .filter(|word| !word.is_empty())
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ");
    truncate_title_utf8(&joined, max_bytes)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::UserMessage;
    use dsh_session::{session_id, SurfaceOp};

    #[test]
    fn fallback_takes_leading_words_within_byte_budget() {
        assert_eq!(
            fallback_session_title("Prove the product headless profile path", 5, 40),
            "Prove the product headless profile"
        );
        assert_eq!(fallback_session_title("  spaced   words  here ", 2, 40), "spaced words");
        assert_eq!(fallback_session_title("日本語のタイトルテスト", 5, 12), "日本語の");
    }

    #[test]
    fn normalize_strips_controls_and_escapes() {
        assert_eq!(
            normalize_session_title("a\u{1B}[31mred\u{1B}[0m b\u{200B}c\td", 80),
            "ared bc d"
        );
        assert_eq!(
            normalize_session_title("\u{1B}]0;evil\u{7}safe", 80),
            "safe"
        );
    }

    #[test]
    fn ensure_fallback_appends_once() {
        let service = SessionTitleService::new(SessionTitleConfig {
            fallback_max_words: 5,
            fallback_max_bytes: 40,
            max_title_bytes: 80,
        })
        .unwrap();
        let session = Session::new(session_id("s"));
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("hello world")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        service.ensure_fallback(&session, 0, "hello world");
        service.ensure_fallback(&session, 0, "hello world");
        let titles: Vec<_> = session
            .events()
            .into_iter()
            .filter(|event| matches!(event.data, SessionEventData::SessionTitle { .. }))
            .collect();
        assert_eq!(titles.len(), 1);
    }

    #[test]
    fn config_rejects_zero_budgets() {
        assert!(SessionTitleService::new(SessionTitleConfig {
            fallback_max_words: 0,
            fallback_max_bytes: 40,
            max_title_bytes: 80,
        })
        .is_err());
    }
}
