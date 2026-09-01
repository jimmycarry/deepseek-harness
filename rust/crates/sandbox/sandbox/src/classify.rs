//! Runner-failure and denial classification for a confined wrap.

use super::RunnerFailureRule;
use std::path::Path;

/// Landlock launcher binary name (the `landlock-run` protocol constant).
pub const LANDLOCK_LAUNCHER_BIN: &str = "landlock-run";

/// Landlock launcher-failure exit (`LAUNCHER_FAILURE_EXIT`).
pub const LANDLOCK_LAUNCHER_FAILURE_EXIT: i32 = 125;

/// Windows ACL restricted-token runner-failure exit.
pub const WINDOWS_ACL_RUNNER_FAILURE_EXIT: i32 = 127;

/// Fatal runner evidence retained for infrastructure-error detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFailureMatch {
    /// The original stderr line that matched a fatal signature.
    pub detail: String,
}

/// Whether `path` is a directory the caller can enter. Checked at
/// classification time, not atomically with spawn.
pub fn is_usable_workdir(path: &str) -> bool {
    let path = Path::new(path);
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Classify a failed run against the selected backend's denial dialect.
pub fn classify_denial(exit_code: Option<i32>, stderr: &str, signatures: &[String]) -> bool {
    matches_signature(exit_code, stderr, signatures)
}

/// Classify one settled process against structured runner-failure rules.
///
/// Each rule requires a nonzero exit, its optional exit-code gate, and a
/// fatal signature on one stderr line after exact informational lines are
/// excluded. Exit status alone never proves runner failure.
pub fn classify_runner_failure(
    exit_code: Option<i32>,
    stderr: &str,
    rules: &[RunnerFailureRule],
) -> Option<RunnerFailureMatch> {
    let code = exit_code?;
    if code == 0 {
        return None;
    }
    let lines = stderr.split('\n').map(|line| line.trim_end_matches('\r'));
    for rule in rules {
        if let Some(allowed) = &rule.allowed_exit_codes {
            if !allowed.contains(&code) {
                continue;
            }
        }
        let informational: Vec<String> = rule
            .informational_lines
            .iter()
            .map(|line| line.to_ascii_lowercase())
            .collect();
        let fatal: Vec<String> = rule
            .fatal_signatures
            .iter()
            .filter(|signature| !signature.trim().is_empty())
            .map(|signature| signature.to_ascii_lowercase())
            .collect();
        for line in lines.clone() {
            let lowered = line.to_ascii_lowercase();
            if informational.iter().any(|item| item == &lowered) {
                continue;
            }
            if fatal.iter().any(|signature| lowered.contains(signature)) {
                return Some(RunnerFailureMatch {
                    detail: line.to_string(),
                });
            }
        }
    }
    None
}

/// Match a non-zero exit against case-insensitive stderr signatures.
pub fn matches_signature(exit_code: Option<i32>, stderr: &str, signatures: &[String]) -> bool {
    let Some(code) = exit_code else {
        return false;
    };
    if code == 0 {
        return false;
    }
    let lowered = stderr.to_ascii_lowercase();
    signatures
        .iter()
        .any(|signature| lowered.contains(&signature.to_ascii_lowercase()))
}

/// bwrap runner-failure rule (signature-only; exit 1 is not reserved).
pub fn bwrap_runner_failure_rules() -> Vec<RunnerFailureRule> {
    vec![RunnerFailureRule {
        allowed_exit_codes: None,
        fatal_signatures: vec!["bwrap: ".into()],
        informational_lines: Vec::new(),
    }]
}

/// Landlock runner-failure rule (exit 125 plus a non-notice fatal line).
pub fn landlock_runner_failure_rules() -> Vec<RunnerFailureRule> {
    vec![RunnerFailureRule {
        allowed_exit_codes: Some(vec![LANDLOCK_LAUNCHER_FAILURE_EXIT]),
        fatal_signatures: vec![format!("{LANDLOCK_LAUNCHER_BIN}: ")],
        informational_lines: vec![format!(
            "{LANDLOCK_LAUNCHER_BIN}: partial enforcement (older Landlock ABI)"
        )],
    }]
}

/// Seatbelt (`sandbox-exec`) runner-failure rule.
pub fn seatbelt_runner_failure_rules() -> Vec<RunnerFailureRule> {
    vec![RunnerFailureRule {
        allowed_exit_codes: None,
        fatal_signatures: vec!["sandbox-exec: ".into()],
        informational_lines: Vec::new(),
    }]
}

/// Windows ACL denial dialect (pwsh/.NET, cmd, and Node EACCES).
pub fn windows_acl_denial_signatures() -> Vec<String> {
    vec![
        "access is denied".into(),
        "access to the path".into(),
        "permission denied".into(),
    ]
}

/// Windows ACL runner-failure rule (exit 127 plus `windows-acl-run: `).
pub fn windows_acl_runner_failure_rules() -> Vec<RunnerFailureRule> {
    vec![RunnerFailureRule {
        allowed_exit_codes: Some(vec![WINDOWS_ACL_RUNNER_FAILURE_EXIT]),
        fatal_signatures: vec!["windows-acl-run: ".into()],
        informational_lines: Vec::new(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn landlock_rules() -> Vec<RunnerFailureRule> {
        landlock_runner_failure_rules()
    }

    #[test]
    fn matches_signature_requires_nonzero_and_dialect() {
        assert!(!matches_signature(
            Some(0),
            "read-only file system",
            &["read-only file system".into()]
        ));
        assert!(!matches_signature(
            None,
            "read-only file system",
            &["read-only file system".into()]
        ));
        assert!(matches_signature(
            Some(1),
            "touch: Read-only file system",
            &["read-only file system".into()]
        ));
        assert!(!matches_signature(
            Some(1),
            "permission denied",
            &["read-only file system".into()]
        ));
    }

    #[test]
    fn classify_runner_failure_ignores_empty_signatures() {
        let notice = "landlock-run: partial enforcement (older Landlock ABI)";
        let empty = [RunnerFailureRule {
            allowed_exit_codes: Some(vec![125]),
            fatal_signatures: vec!["".into(), " ".into(), "\t".into()],
            informational_lines: Vec::new(),
        }];
        assert!(classify_runner_failure(Some(125), "", &empty).is_none());
        assert!(classify_runner_failure(Some(125), notice, &empty).is_none());
    }

    #[test]
    fn classify_runner_failure_keeps_valid_signatures_beside_empty() {
        let notice = "landlock-run: partial enforcement (older Landlock ABI)";
        let fatal = "landlock-run: ruleset creation failed";
        let rules = [RunnerFailureRule {
            allowed_exit_codes: Some(vec![125]),
            fatal_signatures: vec!["".into(), " ".into(), "landlock-run: ".into()],
            informational_lines: vec![notice.into()],
        }];
        let matched = classify_runner_failure(
            Some(125),
            &format!("{notice}\nchild diagnostic\n{fatal}"),
            &rules,
        )
        .expect("fatal line");
        assert_eq!(matched.detail, fatal);
    }

    #[test]
    fn landlock_requires_exit_125_and_a_non_notice_fatal_line() {
        let notice = "landlock-run: partial enforcement (older Landlock ABI)";
        let rules = landlock_rules();
        assert!(classify_runner_failure(Some(1), notice, &rules).is_none());
        assert!(classify_runner_failure(Some(2), notice, &rules).is_none());
        assert!(classify_runner_failure(Some(125), notice, &rules).is_none());
        assert!(classify_runner_failure(Some(125), &notice.to_ascii_uppercase(), &rules).is_none());
        let extra = format!("{notice}: extra detail");
        assert_eq!(
            classify_runner_failure(Some(125), &extra, &rules)
                .expect("notice with extra is fatal")
                .detail,
            extra
        );
        let combined = format!("{notice}\nlandlock-run: exec failed: No such file or directory");
        assert_eq!(
            classify_runner_failure(Some(125), &combined, &rules)
                .expect("fatal after notice")
                .detail,
            "landlock-run: exec failed: No such file or directory"
        );
    }

    #[test]
    fn landlock_future_fatal_diagnostics_fail_closed() {
        let rules = landlock_rules();
        for fatal in [
            "landlock-run: usage error: missing `-- <argv>...` command",
            "landlock-run: landlock is not enforced by this kernel (ABI unsupported or disabled)",
            "landlock-run: cannot open rule path: /gone: No such file or directory",
            "landlock-run: landlock ruleset error: Invalid argument",
            "landlock-run: exec failed: Permission denied",
            "landlock-run: out of memory",
            "landlock-run: future fatal diagnostic",
        ] {
            assert_eq!(
                classify_runner_failure(Some(125), fatal, &rules)
                    .expect(fatal)
                    .detail,
                fatal
            );
        }
    }

    #[test]
    fn windows_acl_denial_signatures_match_the_typescript_dialect() {
        let signatures = windows_acl_denial_signatures();
        assert!(classify_denial(Some(1), "Access is denied.", &signatures));
        assert!(classify_denial(
            Some(1),
            "Access to the path 'C:\\repo\\secret' is denied.",
            &signatures
        ));
        assert!(classify_denial(
            Some(1),
            "Error: EACCES: permission denied, open '/tmp/x'",
            &signatures
        ));
        assert!(!classify_denial(
            Some(1),
            "read-only file system",
            &signatures
        ));
    }

    #[test]
    fn windows_acl_exit_gate_ignores_printed_signature_on_other_exits() {
        let rules = windows_acl_runner_failure_rules();
        assert!(classify_runner_failure(
            Some(3),
            "windows-acl-run: something the command printed",
            &rules
        )
        .is_none());
        assert_eq!(
            classify_runner_failure(Some(127), "windows-acl-run: missing --workspace", &rules)
                .expect("gated 127")
                .detail,
            "windows-acl-run: missing --workspace"
        );
    }

    #[test]
    fn classify_denial_never_treats_clean_exit_or_signal_as_denial() {
        assert!(!classify_denial(
            Some(0),
            "Permission denied",
            &["permission denied".into()]
        ));
        assert!(!classify_denial(
            None,
            "Permission denied",
            &["permission denied".into()]
        ));
    }

    #[test]
    fn usable_workdir_rejects_missing_and_non_directory() {
        assert!(!is_usable_workdir("/no/such/dsh-workdir"));
        let file = std::env::temp_dir().join("dsh-workdir-file");
        std::fs::write(&file, b"x").expect("temp file");
        assert!(!is_usable_workdir(&file.to_string_lossy()));
        let _ = std::fs::remove_file(&file);
        let cwd = std::env::current_dir().expect("cwd");
        assert!(is_usable_workdir(&cwd.to_string_lossy()));
    }
}
