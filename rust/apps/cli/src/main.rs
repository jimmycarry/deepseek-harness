//! `dsh` binary. Product path composes the profile tree, then mounts it.

use dsh_app_boot::{compose_profile, dump_config, register_profile_plugins, shipped_bundles};
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
    let resume_session_id = resume_session_id(&args)?;
    reject_resume_on_stdio(profile, &resume_session_id)?;
    // The stdio server profiles take no positional task; they serve stdin.
    let task = if profile == "acp" || profile == "jsonrpc" {
        None
    } else {
        Some(positional_prompt(&args).ok_or_else(|| {
            "error: a task is required, for example: dsh --profile headless \"run the tests\""
                .to_string()
        })?)
    };
    let layers = shipped_bundles(profile).map_err(|error| error.to_string())?;
    let overlay = replay_overlay();
    let entries =
        compose_profile(&layers, &[], &[], &overlay).map_err(|error| error.to_string())?;
    let ctx = Context::new();
    if let Some(task) = task {
        ctx.provide(Arc::new(HeadlessStartup {
            task: task.to_string(),
            cwd: None,
            resume_session_id,
        }))
        .map_err(|error| error.to_string())?;
    }
    let loader = dsh_cordis_loader::Loader::new();
    register_profile_plugins(&loader);
    loader
        .mount(&ctx, &entries)
        .map_err(|error| error.to_string())?;
    match profile {
        "acp" => dsh_acp::serve_stdio(&ctx).await,
        "jsonrpc" => dsh_sdk_server::serve_stdio(&ctx).await,
        _ => dsh_bundle_headless::run(&ctx).await,
    }
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

fn reject_resume_on_stdio(profile: &str, resume: &Option<String>) -> Result<(), String> {
    if resume.is_some() && (profile == "acp" || profile == "jsonrpc") {
        return Err("error: --resume is a headless option".into());
    }
    Ok(())
}

fn resume_session_id(args: &[String]) -> Result<Option<String>, String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let id = if arg == "--resume" {
            i += 1;
            let value = args.get(i).map(String::as_str).unwrap_or("");
            if value.is_empty() || value.starts_with('-') {
                return Err("error: --resume needs a session id".into());
            }
            Some(value.to_string())
        } else if let Some(value) = arg.strip_prefix("--resume=") {
            if value.is_empty() {
                return Err("error: --resume needs a session id".into());
            }
            Some(value.to_string())
        } else {
            None
        };
        if let Some(id) = id {
            if found.is_some() {
                return Err("error: --resume may be supplied once".into());
            }
            found = Some(id);
        }
        i += 1;
    }
    Ok(found)
}

fn positional_prompt(args: &[String]) -> Option<&str> {
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg == "--profile" || arg == "--resume" {
            skip_value = true;
            continue;
        }
        if arg.starts_with("--profile=") || arg.starts_with("--resume=") {
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if *arg == "headless" || *arg == "web" || *arg == "acp" || *arg == "jsonrpc" {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn print_dump(args: &[String]) -> Result<(), String> {
    let profile = profile_of(args).unwrap_or("headless");
    let layers = shipped_bundles(profile).map_err(|error| error.to_string())?;
    let entries = compose_profile(&layers, &[], &[], &[]).map_err(|error| error.to_string())?;
    print!("{}", dump_config(&entries));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn positional_prompt_skips_resume_and_profile_values() {
        assert_eq!(
            positional_prompt(&args(&[
                "--profile",
                "headless",
                "--resume",
                "abc",
                "do the thing",
            ])),
            Some("do the thing")
        );
        assert_eq!(
            positional_prompt(&args(&["--resume=abc", "next"])),
            Some("next")
        );
    }

    #[test]
    fn resume_session_id_reads_space_and_equals_forms() {
        assert_eq!(
            resume_session_id(&args(&["--resume", "abc", "task"])).unwrap(),
            Some("abc".into())
        );
        assert_eq!(
            resume_session_id(&args(&["--resume=abc", "task"])).unwrap(),
            Some("abc".into())
        );
        assert!(resume_session_id(&args(&["--resume"])).unwrap_err().contains("needs a session id"));
        assert!(resume_session_id(&args(&["--resume", "--profile"]))
            .unwrap_err()
            .contains("needs a session id"));
        assert!(resume_session_id(&args(&["--resume", "a", "--resume", "b"]))
            .unwrap_err()
            .contains("once"));
    }

    #[test]
    fn resume_is_rejected_on_stdio_profiles() {
        assert!(reject_resume_on_stdio("acp", &Some("abc".into()))
            .unwrap_err()
            .contains("headless"));
        assert!(reject_resume_on_stdio("jsonrpc", &Some("abc".into())).is_err());
        assert!(reject_resume_on_stdio("headless", &Some("abc".into())).is_ok());
        assert!(reject_resume_on_stdio("acp", &None).is_ok());
    }
}
