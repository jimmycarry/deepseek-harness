//! Built-bin smoke: `dsh --resume <id>` continues a persisted JSONL session.

use std::process::Command;

#[test]
fn headless_resume_continues_the_persisted_log() {
    let home = std::env::temp_dir().join(format!(
        "dsh-resume-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let first = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args(["--profile", "headless", "first question"])
        .env("DSH_HOME", &home)
        .env("DSH_REPLAY_TEXT", "first-answer")
        .output()
        .expect("first dsh");
    assert!(
        first.status.success(),
        "first run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("first-answer"));

    let sessions = home.join("sessions");
    let mut logs: Vec<_> = std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "jsonl").then_some(path)
        })
        .collect();
    assert_eq!(logs.len(), 1, "one session jsonl after the first run");
    let log = logs.remove(0);
    let id = log.file_stem().unwrap().to_string_lossy().into_owned();

    let second = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args([
            "--profile",
            "headless",
            "--resume",
            &id,
            "second question",
        ])
        .env("DSH_HOME", &home)
        .env("DSH_REPLAY_TEXT", "second-answer")
        .output()
        .expect("second dsh");
    assert!(
        second.status.success(),
        "resume run failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("second-answer"));

    let body = std::fs::read_to_string(&log).expect("resumed jsonl");
    assert!(body.contains("\"type\":\"session/end-seed\""), "{body}");
    assert!(body.contains("first question"), "{body}");
    assert!(body.contains("second question"), "{body}");
    assert!(body.contains("\"turn\":1"), "{body}");
    assert!(body.contains("\"turn\":2"), "{body}");
    assert!(body.contains("\"reason\":\"resume\""), "{body}");

    let _ = std::fs::remove_dir_all(&home);
}
