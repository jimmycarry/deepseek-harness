use super::*;
use dsh_session::{event_type_name, session_id, Session, SessionEventData};
use serde_json::json;

fn out(over: HookOutput) -> HookOutput {
    over
}

fn base() -> HookOutput {
    HookOutput {
        exit_code: Some(0),
        stderr: String::new(),
        stdout: String::new(),
        ..HookOutput::default()
    }
}

#[test]
fn match_all_sentinels() {
    for mode in [MatcherMode::ClaudeCode, MatcherMode::Codex] {
        assert!(matches_matcher(None, "Bash", mode));
        assert!(matches_matcher(Some(""), "anything", mode));
        assert!(matches_matcher(Some("*"), "whatever", mode));
    }
}

#[test]
fn claude_literal_and_regex() {
    assert!(matches_matcher(
        Some("Bash"),
        "Bash",
        MatcherMode::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some("Bash"),
        "BashOutput",
        MatcherMode::ClaudeCode
    ));
    assert!(matches_matcher(
        Some("Edit|Write"),
        "Edit",
        MatcherMode::ClaudeCode
    ));
    assert!(matches_matcher(
        Some("Edit|Write"),
        "Write",
        MatcherMode::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some("Edit|Write"),
        "Read",
        MatcherMode::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some("Edit|Write"),
        "EditFile",
        MatcherMode::ClaudeCode
    ));
    assert!(matches_matcher(
        Some("^Bash$"),
        "Bash",
        MatcherMode::ClaudeCode
    ));
    assert!(matches_matcher(
        Some("Bash.*"),
        "BashOutput",
        MatcherMode::ClaudeCode
    ));
    assert!(matches_matcher(
        Some(r".*\.ts$"),
        "foo.ts",
        MatcherMode::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some(r".*\.ts$"),
        "foo.js",
        MatcherMode::ClaudeCode
    ));
}

#[test]
fn codex_always_regex() {
    assert!(matches_matcher(Some("Bash"), "Bash", MatcherMode::Codex));
    assert!(matches_matcher(
        Some("Bash"),
        "BashOutput",
        MatcherMode::Codex
    ));
    assert!(matches_matcher(
        Some("Edit|Write"),
        "Edit",
        MatcherMode::Codex
    ));
    assert!(matches_matcher(Some("^Bash$"), "Bash", MatcherMode::Codex));
    assert!(!matches_matcher(
        Some("^Bash$"),
        "BashOutput",
        MatcherMode::Codex
    ));
}

#[test]
fn invalid_regex_is_non_match() {
    assert!(!matches_matcher(Some("("), "x", MatcherMode::ClaudeCode));
    assert!(!matches_matcher(Some("["), "x", MatcherMode::Codex));
}

#[test]
fn matcher_diagnostics() {
    assert!(matcher_diagnostic(None, MatcherMode::ClaudeCode).is_none());
    assert!(matcher_diagnostic(Some(""), MatcherMode::Codex).is_none());
    assert!(matcher_diagnostic(Some("*"), MatcherMode::Codex).is_none());
    assert!(matcher_diagnostic(Some("Edit|Write"), MatcherMode::ClaudeCode).is_none());
    assert!(matcher_diagnostic(Some("^Bash$"), MatcherMode::ClaudeCode).is_none());
    assert_eq!(
        matcher_diagnostic(Some("("), MatcherMode::ClaudeCode).as_deref(),
        Some(r#"invalid claude-code regex matcher "(""#)
    );
    assert_eq!(
        matcher_diagnostic(Some("["), MatcherMode::Codex).as_deref(),
        Some(r#"invalid codex regex matcher "[""#)
    );
}

#[test]
fn parse_exit_semantics() {
    let out = parse_hook_output(Some(0), "", "", None);
    assert_eq!(out.exit_code, Some(0));
    assert!(out.decision.is_none());

    let blocked = parse_hook_output(Some(2), "", "this command is not allowed", None);
    assert_eq!(blocked.decision, Some(HookDecision::Block));
    assert_eq!(
        blocked.reason.as_deref(),
        Some("this command is not allowed")
    );

    let empty = parse_hook_output(Some(2), "", "   ", None);
    assert_eq!(empty.decision, Some(HookDecision::Block));
    assert!(empty.reason.is_none());

    let warn = parse_hook_output(Some(1), "", "some warning", None);
    assert!(warn.decision.is_none());
    assert_eq!(warn.stderr, "some warning");

    let missing = parse_hook_output(None, "", "spawn failed: ENOENT", None);
    assert!(missing.exit_code.is_none());
    assert!(missing.decision.is_none());
}

#[test]
fn parse_structured_stdout() {
    let out = parse_hook_output(
        Some(0),
        &json!({"continue": false, "stopReason": "budget exceeded", "systemMessage": "heads up"})
            .to_string(),
        "",
        None,
    );
    assert_eq!(out.continue_run, Some(false));
    assert_eq!(out.stop_reason.as_deref(), Some("budget exceeded"));
    assert_eq!(out.system_message.as_deref(), Some("heads up"));

    assert_eq!(
        parse_hook_output(
            Some(0),
            &json!({"decision": "block", "reason": "nope"}).to_string(),
            "",
            None
        )
        .decision,
        Some(HookDecision::Block)
    );
    assert_eq!(
        parse_hook_output(
            Some(0),
            &json!({"decision": "approve"}).to_string(),
            "",
            None
        )
        .decision,
        Some(HookDecision::Approve)
    );
    assert!(
        parse_hook_output(Some(0), &json!({"decision": "deny"}).to_string(), "", None)
            .decision
            .is_none()
    );
    assert!(
        parse_hook_output(Some(0), &json!({"decision": "allow"}).to_string(), "", None)
            .decision
            .is_none()
    );
    assert!(
        parse_hook_output(Some(0), &json!({"decision": "ask"}).to_string(), "", None)
            .decision
            .is_none()
    );

    let deny = parse_hook_output(
        Some(0),
        &json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}})
            .to_string(),
        "",
        None,
    );
    assert_eq!(deny.hook_event_name.as_deref(), Some("PreToolUse"));
    assert_eq!(deny.decision, Some(HookDecision::Deny));

    let override_decision = parse_hook_output(
        Some(0),
        &json!({
            "decision": "approve",
            "hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "denied by policy"}
        })
        .to_string(),
        "",
        None,
    );
    assert_eq!(override_decision.decision, Some(HookDecision::Deny));
    assert_eq!(
        override_decision.reason.as_deref(),
        Some("denied by policy")
    );

    let mismatch = parse_hook_output(
        Some(0),
        &json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "no",
            "additionalContext": "x",
            "updatedInput": {"command": "y"}
        }})
        .to_string(),
        "",
        Some("Stop"),
    );
    assert_eq!(mismatch.hook_event_name.as_deref(), Some("PreToolUse"));
    assert!(mismatch.decision.is_none());
    assert!(mismatch.reason.is_none());
    assert!(mismatch.additional_context.is_none());
    assert!(mismatch.updated_input.is_none());

    let match_ok = parse_hook_output(
        Some(0),
        &json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "additionalContext": "x"
        }})
        .to_string(),
        "",
        Some("PreToolUse"),
    );
    assert_eq!(match_ok.decision, Some(HookDecision::Deny));
    assert_eq!(match_ok.additional_context.as_deref(), Some("x"));

    let no_name = parse_hook_output(
        Some(0),
        &json!({"hookSpecificOutput": {"permissionDecision": "deny", "additionalContext": "x"}})
            .to_string(),
        "",
        Some("Stop"),
    );
    assert!(no_name.decision.is_none());
    assert!(no_name.additional_context.is_none());

    let top_survives = parse_hook_output(
        Some(0),
        &json!({
            "decision": "block",
            "reason": "top",
            "continue": false,
            "stopReason": "halt",
            "hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}
        })
        .to_string(),
        "",
        Some("Stop"),
    );
    assert_eq!(top_survives.decision, Some(HookDecision::Block));
    assert_eq!(top_survives.reason.as_deref(), Some("top"));
    assert_eq!(top_survives.continue_run, Some(false));

    let malformed = parse_hook_output(Some(0), "{ not valid json", "", None);
    assert!(malformed.decision.is_none());
    assert_eq!(
        parse_hook_output(Some(0), "just some text output", "", None).stdout,
        "just some text output"
    );
    assert_eq!(parse_hook_output(Some(0), "", "", None).stdout, "");
    assert!(parse_hook_output(Some(0), "[1,2,3]", "", None)
        .decision
        .is_none());

    let exit2 = parse_hook_output(
        Some(2),
        &json!({"decision": "approve"}).to_string(),
        "blocked",
        None,
    );
    assert_eq!(exit2.decision, Some(HookDecision::Block));
    assert_eq!(exit2.reason.as_deref(), Some("blocked"));
}

#[test]
fn merge_precedence_and_reasons() {
    let empty = merge_hook_outputs(&[]);
    assert_eq!(empty.decision, MergedDecision::None);
    assert!(!empty.stop);

    let mut allow = base();
    allow.decision = Some(HookDecision::Allow);
    assert_eq!(
        merge_hook_outputs(&[allow.clone()]).decision,
        MergedDecision::Allow
    );
    let mut approve = base();
    approve.decision = Some(HookDecision::Approve);
    assert_eq!(
        merge_hook_outputs(&[approve]).decision,
        MergedDecision::Allow
    );

    let mut ask = base();
    ask.decision = Some(HookDecision::Ask);
    let mut deny = base();
    deny.decision = Some(HookDecision::Deny);
    assert_eq!(
        merge_hook_outputs(&[allow.clone(), ask.clone()]).decision,
        MergedDecision::Ask
    );
    assert_eq!(
        merge_hook_outputs(&[ask.clone(), deny.clone()]).decision,
        MergedDecision::Deny
    );

    let mut first = base();
    first.decision = Some(HookDecision::Deny);
    first.reason = Some("first objection".into());
    let mut allowed = base();
    allowed.decision = Some(HookDecision::Allow);
    allowed.reason = Some("this allow reason is NOT collected".into());
    let mut second = base();
    second.decision = Some(HookDecision::Block);
    second.reason = Some("second objection".into());
    assert_eq!(
        merge_hook_outputs(&[first, allowed, second])
            .reason
            .as_deref(),
        Some("first objection\n\nsecond objection")
    );

    let mut ask_r = base();
    ask_r.decision = Some(HookDecision::Ask);
    ask_r.reason = Some("needs approval".into());
    let mut allow_r = base();
    allow_r.decision = Some(HookDecision::Allow);
    allow_r.reason = Some("allow reason — not surfaced".into());
    let merged = merge_hook_outputs(&[allow_r, ask_r]);
    assert_eq!(merged.decision, MergedDecision::Ask);
    assert_eq!(merged.reason.as_deref(), Some("needs approval"));

    let mut halt = base();
    halt.continue_run = Some(false);
    halt.stop_reason = Some("halt now".into());
    let mut later = base();
    later.continue_run = Some(false);
    later.stop_reason = Some("second halt — ignored".into());
    let mut cont = base();
    cont.continue_run = Some(true);
    let stopped = merge_hook_outputs(&[cont, halt, later]);
    assert!(stopped.stop);
    assert_eq!(stopped.stop_reason.as_deref(), Some("halt now"));

    let mut a = base();
    a.additional_context = Some("ctx-A".into());
    a.system_message = Some("warn-A".into());
    let mut empty_ctx = base();
    empty_ctx.additional_context = Some(String::new());
    empty_ctx.system_message = Some(String::new());
    let mut b = base();
    b.additional_context = Some("ctx-B".into());
    let mut c = base();
    c.system_message = Some("warn-B".into());
    let collected = merge_hook_outputs(&[a, empty_ctx, b, c]);
    assert_eq!(collected.additional_context, ["ctx-A", "ctx-B"]);
    assert_eq!(collected.system_messages, ["warn-A", "warn-B"]);
    let _ = out(base());
}

#[test]
fn hook_events_are_log_only() {
    let session = Session::new(session_id("s"));
    append_hook_invoked(
        &session,
        HookInvocation {
            turn: 1,
            point: "PreToolUse".into(),
            dialect: HookDialect::ClaudeCode,
            handler_id: "h1".into(),
            matcher: Some("Bash".into()),
        },
    );
    let events = session.events();
    let invoked = events
        .iter()
        .find(|event| event_type_name(&event.data) == "hook/invoked")
        .expect("invoked");
    assert!(invoked.surface_op.is_none());
    let SessionEventData::Extension { data, .. } = &invoked.data else {
        panic!("extension");
    };
    assert_eq!(data["dialect"], "claude-code");
    assert_eq!(data["matcher"], "Bash");

    let session2 = Session::new(session_id("s2"));
    append_hook_invoked(
        &session2,
        HookInvocation {
            turn: 2,
            point: "Stop".into(),
            dialect: HookDialect::Codex,
            handler_id: "h2".into(),
            matcher: None,
        },
    );
    let SessionEventData::Extension { data, .. } = &session2.events()[0].data else {
        panic!("extension");
    };
    assert!(data.get("matcher").is_none());

    let session3 = Session::new(session_id("s3"));
    let mut blocked = base();
    blocked.exit_code = Some(2);
    blocked.stderr = "blocked".into();
    blocked.decision = Some(HookDecision::Deny);
    append_hook_result(
        &session3,
        HookResultRecord {
            turn: 1,
            point: "PreToolUse".into(),
            handler_id: "h1".into(),
            output: blocked,
            stderr_summary_max_chars: 500,
            duration_ms: 5,
        },
    );
    let SessionEventData::Extension { data, .. } = &session3.events()[0].data else {
        panic!("extension");
    };
    assert_eq!(data["decision"], "deny");
    assert_eq!(data["exitCode"], 2);
    assert_eq!(data["stderrSummary"], "blocked");

    let session4 = Session::new(session_id("s4"));
    let mut halt = base();
    halt.continue_run = Some(false);
    append_hook_result(
        &session4,
        HookResultRecord {
            turn: 1,
            point: "Stop".into(),
            handler_id: "halt".into(),
            output: halt,
            stderr_summary_max_chars: 500,
            duration_ms: 5,
        },
    );
    append_hook_result(
        &session4,
        HookResultRecord {
            turn: 1,
            point: "Stop".into(),
            handler_id: "noop".into(),
            output: base(),
            stderr_summary_max_chars: 500,
            duration_ms: 5,
        },
    );
    let decisions: Vec<_> = session4
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            SessionEventData::Extension { data, .. } => Some((
                data["handlerId"].as_str().unwrap().to_string(),
                data["decision"].as_str().unwrap().to_string(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![
            ("halt".into(), "stop".into()),
            ("noop".into(), "pass".into())
        ]
    );

    assert_eq!(
        summarize_stderr(&format!("  {}  ", "x".repeat(600)), 500).as_deref(),
        Some(format!("{}…", "x".repeat(500)).as_str())
    );
}

#[tokio::test]
async fn run_hook_never_throws_and_frames_stdin() {
    let hook = CommandHook {
        command: "true".into(),
        timeout_sec: Some(2.0),
    };
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = captured.clone();
    let result = run_hook(
        move |request| {
            *slot.lock().expect("slot") = Some(request);
            async move {
                Ok(HookShellResult {
                    exit_code: Some(0),
                    stdout: json!({"decision": "block"}).to_string(),
                    stderr: String::new(),
                })
            }
        },
        &hook,
        RunHookOptions {
            payload: json!({"hook_event_name": "PreToolUse"}),
            env: None,
            cwd: None,
            aborted: false,
            trailing_newline: true,
            default_timeout_ms: 600_000,
            expected_event_name: Some("PreToolUse".into()),
        },
        || 10,
    )
    .await;
    let request = captured.lock().expect("slot").clone().expect("ran");
    assert!(request.stdin.ends_with('\n'));
    assert_eq!(request.timeout_ms, 2000);
    assert_eq!(result.output.decision, Some(HookDecision::Block));

    let failed = run_hook(
        |_request| async { Err("unusable workdir".into()) },
        &hook,
        RunHookOptions {
            payload: json!({}),
            env: None,
            cwd: None,
            aborted: false,
            trailing_newline: false,
            default_timeout_ms: 600_000,
            expected_event_name: None,
        },
        || 0,
    )
    .await;
    assert!(failed.output.exit_code.is_none());
    assert_eq!(failed.output.stderr, "unusable workdir");
}

#[tokio::test]
async fn detached_drain_sets_abort() {
    let detached = create_detached_runs();
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = flag.clone();
    detached.track(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    detached.drain().await;
    assert!(detached.is_aborted());
    assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
}
