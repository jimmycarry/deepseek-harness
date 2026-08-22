//! `dsh` binary. Product path composes the profile tree, then mounts it.

use dsh_app_boot::{
    compose_profile, dump_config, register_profile_plugins, shipped_bundles,
};
use dsh_bundle_headless::HeadlessStartup;
use dsh_cordis::Context;
use dsh_cordis_loader::{Entry, EntryPatch};
use serde_json::Value;
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
        print_dump(&args)?;
        return Ok(());
    }
    let profile = profile_of(&args).unwrap_or("headless");
    let task = positional_prompt(&args).ok_or_else(|| {
        "error: a task is required, for example: dsh --profile headless \"run the tests\""
            .to_string()
    })?;
    let layers = shipped_bundles(profile).map_err(|error| error.to_string())?;
    let overlay = replay_overlay();
    let entries = compose_profile(&layers, &[], &[], &overlay).map_err(|error| error.to_string())?;
    let ctx = Context::new();
    ctx.provide(Arc::new(HeadlessStartup {
        task: task.to_string(),
    }))
    .map_err(|error| error.to_string())?;
    let loader = dsh_cordis_loader::Loader::new();
    register_profile_plugins(&loader);
    loader
        .mount(&ctx, &entries)
        .map_err(|error| error.to_string())?;
    dsh_bundle_headless::run(&ctx).await
}

fn replay_overlay() -> Vec<EntryPatch> {
    if env::var("DEEPSEEK_API_KEY").is_ok() && env::var("DSH_REPLAY_TEXT").is_err() {
        return Vec::new();
    }
    let text = env::var("DSH_REPLAY_TEXT").unwrap_or_else(|_| "pong".into());
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(serde_json::json!({ "text": text }));
    vec![disable, EntryPatch::insert_row(replay)]
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

fn print_dump(args: &[String]) -> Result<(), String> {
    let profile = profile_of(args).unwrap_or("headless");
    let layers = shipped_bundles(profile).map_err(|error| error.to_string())?;
    let entries = compose_profile(&layers, &[], &[], &[]).map_err(|error| error.to_string())?;
    print!("{}", dump_config(&entries));
    Ok(())
}
