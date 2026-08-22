//! `dsh` binary. Product path runs the compiled artifact, not a source hook.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_agent_spine::{apply, apply_replay};
use dsh_app_boot::{compose_profile, dump_config, profile_templates, BundleLayer};
use dsh_cordis::Context;
use dsh_cordis_loader::{EntryPatch, Loader};
use dsh_llm::{ContentBlock, UserMessage};
use dsh_llm_deepseek::DeepSeekAdapter;
use dsh_session::SessionStore;
use dsh_session_persistence_jsonl::write_jsonl;
use std::env;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(error) = run(env::args().skip(1).collect()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--dump-config") {
        print_dump();
        return Ok(());
    }
    let profile = profile_of(&args).unwrap_or("headless");
    let prompt = positional_prompt(&args).unwrap_or("hello");
    let ctx = Context::new();
    if env::var("DSH_REPLAY_TEXT").is_ok() || env::var("DEEPSEEK_API_KEY").is_err() {
        let text = env::var("DSH_REPLAY_TEXT").unwrap_or_else(|_| "pong".into());
        apply_replay(&ctx, &text).map_err(|error| error.to_string())?;
    } else {
        let adapter = DeepSeekAdapter::from_env().map_err(|error| error.to_string())?;
        apply(&ctx, Arc::new(adapter)).map_err(|error| error.to_string())?;
    }
    let _ = profile;
    let session = ctx
        .service::<SessionStore>()
        .map_err(|error| error.to_string())?
        .create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .map_err(|error| error.to_string())?
        .create(Arc::clone(&session))
        .map_err(|error| error.to_string())?;
    run_followup(
        handle.agent.as_ref(),
        UserMessage {
            content: vec![ContentBlock::text(prompt)],
            source: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    if let Some(text) = handle.agent.session().last_assistant_text() {
        println!("{text}");
    }
    if let Ok(path) = env::var("DSH_SESSION_JSONL") {
        write_jsonl(&path, handle.agent.session().as_ref())
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn profile_of(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|window| window[0] == "--profile")
        .map(|window| window[1].as_str())
}

fn positional_prompt(args: &[String]) -> Option<&str> {
    args.iter()
        .filter(|arg| !arg.starts_with('-'))
        .find(|arg| *arg != "headless" && *arg != "web")
        .map(String::as_str)
}

fn print_dump() {
    let base = BundleLayer {
        name: "dsh-base".into(),
        patches: vec![insert("llm", "dsh-llm"), insert("session", "dsh-session")],
    };
    let headless = BundleLayer {
        name: "dsh-headless".into(),
        patches: vec![insert("runner", "dsh-headless-runner")],
    };
    let entries = compose_profile(&[base, headless], &[], &[], &[]).expect("compose");
    print!("{}", dump_config(&entries));
    let _ = Loader::new();
    let _ = profile_templates();
}

fn insert(id: &str, name: &str) -> EntryPatch {
    EntryPatch {
        id: Some(id.into()),
        name: Some(name.into()),
        config: None,
        disabled: None,
        insert: true,
    }
}
