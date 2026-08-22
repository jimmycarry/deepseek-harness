//! User-questions capability (`ctx.userQuestions`).
//!
//! The service is mounted by the base bundle; a UI assembly registers the one
//! provider that can actually reach a human. `ask` without a provider fails
//! with `no user-questions provider is registered`, which automation
//! assemblies surface verbatim.

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use std::sync::{Arc, Mutex};

/// One structured ask presented to the human.
#[derive(Debug, Clone)]
pub struct UserQuestion {
    /// Stable ask identity (`plan-review`).
    pub id: String,
    /// Short header shown above the question.
    pub header: String,
    /// Question text.
    pub question: String,
    /// Closed answer options in display order.
    pub options: Vec<String>,
}

/// Provider-returned answer.
#[derive(Debug, Clone)]
pub struct UserQuestionReply {
    /// Chosen option, verbatim from [`UserQuestion::options`].
    pub choice: String,
    /// Free-form feedback typed alongside the choice, when the UI offers one.
    pub feedback: Option<String>,
}

/// The one UI channel able to present asks.
#[async_trait]
pub trait UserQuestionProvider: Send + Sync {
    /// Present `question` and resolve with the human's reply.
    ///
    /// # Errors
    /// A dismissed ask or an unreachable channel, as provider-defined text.
    async fn ask(&self, question: UserQuestion) -> Result<UserQuestionReply, String>;
}

/// `ctx.userQuestions`.
#[derive(Default)]
pub struct UserQuestionsService {
    provider: Arc<Mutex<Option<Arc<dyn UserQuestionProvider>>>>,
}

impl UserQuestionsService {
    /// Create the service with no provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide `ctx.userQuestions`.
    ///
    /// # Errors
    /// A duplicate service registration.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<Self>> {
        let service = Arc::new(Self::new());
        ctx.provide(Arc::clone(&service))?;
        Ok(service)
    }

    /// Register the UI provider; the returned closure removes it.
    pub fn register(&self, provider: Arc<dyn UserQuestionProvider>) -> impl FnOnce() + Send {
        *self.provider.lock().expect("user-questions provider") = Some(provider);
        let slot = Arc::clone(&self.provider);
        move || {
            *slot.lock().expect("user-questions provider") = None;
        }
    }

    /// Present `question` through the registered provider.
    ///
    /// # Errors
    /// `no user-questions provider is registered` when no UI is mounted, or
    /// the provider's own failure text.
    pub async fn ask(&self, question: UserQuestion) -> Result<UserQuestionReply, String> {
        let provider = self
            .provider
            .lock()
            .expect("user-questions provider")
            .clone();
        match provider {
            Some(provider) => provider.ask(question).await,
            None => Err("no user-questions provider is registered".into()),
        }
    }
}

impl Service for UserQuestionsService {
    const KEY: &'static str = "userQuestions";
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-user-questions"
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Approver;

    #[async_trait]
    impl UserQuestionProvider for Approver {
        async fn ask(&self, question: UserQuestion) -> Result<UserQuestionReply, String> {
            Ok(UserQuestionReply {
                choice: question.options[0].clone(),
                feedback: None,
            })
        }
    }

    #[tokio::test]
    async fn ask_without_provider_fails_with_the_exact_sentence() {
        let service = UserQuestionsService::new();
        let err = service
            .ask(UserQuestion {
                id: "plan-review".into(),
                header: "Plan review".into(),
                question: "Approve this plan and leave plan mode?".into(),
                options: vec!["Approve".into(), "Keep planning".into()],
            })
            .await
            .unwrap_err();
        assert_eq!(err, "no user-questions provider is registered");
    }

    #[tokio::test]
    async fn registered_provider_answers() {
        let ctx = Context::new();
        let service = UserQuestionsService::install(&ctx).unwrap();
        let _disposer = service.register(Arc::new(Approver));
        let reply = service
            .ask(UserQuestion {
                id: "plan-review".into(),
                header: "Plan review".into(),
                question: "Approve this plan and leave plan mode?".into(),
                options: vec!["Approve".into(), "Keep planning".into()],
            })
            .await
            .unwrap();
        assert_eq!(reply.choice, "Approve");
        assert!(ctx.has_service("userQuestions"));
    }
}
