//! Documented Node runner delegation for the TypeScript windows-acl argv contract.
//!
//! Rust does not call `CreateRestrictedToken` or mutate DACLs. Windows file
//! effects are enforced by prefixing the caller's argv with the existing
//! `@deepseek-ai/dsh-sandbox-windows-acl` Node runner (`lib/runner.js`, or
//! `src/runner.ts` through `tsx`). Wine and Linux cannot prove NTFS/DACL;
//! native Windows owns the real-runner suites.

use crate::{temp_write_sid, workspace_write_sid};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Printed on every runner-side failure. The seam matches this prefix.
pub const RUNNER_SIGNATURE: &str = "windows-acl-run";

/// Exit code for every runner-side failure. The child is never spawned unrestricted.
pub const RUNNER_FAILURE_EXIT: i32 = 127;

/// Modes the Node runner accepts (`danger-full-access` is not a runner mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsAclRunnerMode {
    /// No capability-SID grants.
    ReadOnly,
    /// Workspace and private-temp capability SIDs.
    WorkspaceWrite,
}

impl WindowsAclRunnerMode {
    /// TypeScript mode string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }

    /// Parse a TypeScript runner mode string.
    pub fn parse(mode: &str) -> Option<Self> {
        match mode {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            _ => None,
        }
    }
}

/// How the seam locates the TypeScript runner entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsAclRunnerEntry {
    /// Production `lib/runner.js`.
    BuiltJs(PathBuf),
    /// Development `src/runner.ts` launched through tsx.
    SourceTs(PathBuf),
}

/// `[node, runner]` or `[node, --import, tsx/esm, runner.ts]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsAclRunnerInvocation {
    /// Node executable (`process.execPath` in TypeScript).
    pub node: PathBuf,
    /// Built or source runner entry.
    pub entry: WindowsAclRunnerEntry,
}

/// One confinement request after the invocation prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsAclRunnerRequest {
    /// `--workspace` directory.
    pub workspace: String,
    /// `--temp` directory (private temp, or the ambient temp root when agentless).
    pub temp: String,
    /// `--mode`.
    pub mode: WindowsAclRunnerMode,
    /// Seam-managed workspace write SID. Must arrive with [`Self::temp_write_sid`].
    pub write_sid: Option<String>,
    /// Seam-managed private-temp write SID.
    pub temp_write_sid: Option<String>,
    /// Wrapped command after `--`. Must be non-empty.
    pub command: Vec<String>,
}

/// Runner-argv refusal. [`Self::signature_line`] is the stderr contract.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct WindowsAclRunnerError(pub String);

impl WindowsAclRunnerError {
    /// `windows-acl-run: <detail>` line the seam classifies as runner failure.
    pub fn signature_line(&self) -> String {
        format!("{RUNNER_SIGNATURE}: {}", self.0)
    }
}

/// Prefer `lib/runner.js` when present, else `src/runner.ts`.
pub fn resolve_windows_acl_runner_entry(package_root: &Path) -> Option<WindowsAclRunnerEntry> {
    let built = package_root.join("lib/runner.js");
    if built.is_file() {
        return Some(WindowsAclRunnerEntry::BuiltJs(built));
    }
    let source = package_root.join("src/runner.ts");
    if source.is_file() {
        return Some(WindowsAclRunnerEntry::SourceTs(source));
    }
    None
}

/// TypeScript `windowsAclRunnerInvocation` prefix.
pub fn windows_acl_runner_prefix(invocation: &WindowsAclRunnerInvocation) -> Vec<String> {
    match &invocation.entry {
        WindowsAclRunnerEntry::BuiltJs(path) => {
            vec![
                invocation.node.display().to_string(),
                path.display().to_string(),
            ]
        }
        WindowsAclRunnerEntry::SourceTs(path) => vec![
            invocation.node.display().to_string(),
            "--import".into(),
            "tsx/esm".into(),
            path.display().to_string(),
        ],
    }
}

/// Flags the Node runner parses (`--workspace` … `--` plus the wrapped argv).
///
/// # Errors
/// Same refusal details as the TypeScript runner (`fail(...)`).
pub fn windows_acl_runner_flags(
    request: &WindowsAclRunnerRequest,
) -> Result<Vec<String>, WindowsAclRunnerError> {
    validate_windows_acl_runner_request(request)?;
    let mut flags = vec![
        "--workspace".into(),
        request.workspace.clone(),
        "--temp".into(),
        request.temp.clone(),
        "--mode".into(),
        request.mode.as_str().to_string(),
    ];
    if let (Some(write_sid), Some(temp_write_sid)) = (&request.write_sid, &request.temp_write_sid) {
        flags.extend([
            "--write-sid".into(),
            write_sid.clone(),
            "--temp-write-sid".into(),
            temp_write_sid.clone(),
        ]);
    }
    flags.push("--".into());
    flags.extend(request.command.iter().cloned());
    Ok(flags)
}

/// Invocation prefix plus the runner flags.
///
/// # Errors
/// [`windows_acl_runner_flags`] refusals.
pub fn windows_acl_runner_argv(
    invocation: &WindowsAclRunnerInvocation,
    request: &WindowsAclRunnerRequest,
) -> Result<Vec<String>, WindowsAclRunnerError> {
    let mut argv = windows_acl_runner_prefix(invocation);
    argv.extend(windows_acl_runner_flags(request)?);
    Ok(argv)
}

/// TypeScript `parseArgs` over the tokens after `node` / runner / tsx.
///
/// # Errors
/// Same missing-value / unknown-argument / missing-field details as TypeScript.
pub fn parse_windows_acl_runner_args(
    raw: &[String],
) -> Result<WindowsAclRunnerRequest, WindowsAclRunnerError> {
    let mut workspace = None;
    let mut temp = None;
    let mut mode = None;
    let mut write_sid = None;
    let mut temp_write_sid = None;
    let mut index = 0;
    while index < raw.len() {
        let token = &raw[index];
        if token == "--" {
            index += 1;
            break;
        }
        index += 1;
        let value = raw
            .get(index)
            .ok_or_else(|| WindowsAclRunnerError(format!("missing value after {token}")))?;
        match token.as_str() {
            "--workspace" => workspace = Some(value.clone()),
            "--temp" => temp = Some(value.clone()),
            "--mode" => mode = Some(value.clone()),
            "--write-sid" => write_sid = Some(value.clone()),
            "--temp-write-sid" => temp_write_sid = Some(value.clone()),
            other => {
                return Err(WindowsAclRunnerError(format!("unknown argument: {other}")));
            }
        }
        index += 1;
    }
    let workspace = workspace.ok_or_else(|| WindowsAclRunnerError("missing --workspace".into()))?;
    let temp = temp.ok_or_else(|| WindowsAclRunnerError("missing --temp".into()))?;
    let mode_raw = mode.ok_or_else(|| WindowsAclRunnerError("unknown mode: undefined".into()))?;
    let mode = WindowsAclRunnerMode::parse(&mode_raw)
        .ok_or_else(|| WindowsAclRunnerError(format!("unknown mode: {mode_raw}")))?;
    let command = raw[index..].to_vec();
    if command.is_empty() {
        return Err(WindowsAclRunnerError("missing command after --".into()));
    }
    let request = WindowsAclRunnerRequest {
        workspace,
        temp,
        mode,
        write_sid,
        temp_write_sid,
        command,
    };
    validate_windows_acl_runner_request(&request)?;
    Ok(request)
}

fn validate_windows_acl_runner_request(
    request: &WindowsAclRunnerRequest,
) -> Result<(), WindowsAclRunnerError> {
    if request.command.is_empty() {
        return Err(WindowsAclRunnerError("missing command after --".into()));
    }
    let seam_managed = request.write_sid.is_some() || request.temp_write_sid.is_some();
    if request.mode == WindowsAclRunnerMode::ReadOnly && seam_managed {
        return Err(WindowsAclRunnerError(
            "read-only does not accept --write-sid or --temp-write-sid".into(),
        ));
    }
    if request.mode == WindowsAclRunnerMode::WorkspaceWrite
        && request.write_sid.is_some() != request.temp_write_sid.is_some()
    {
        return Err(WindowsAclRunnerError(
            "workspace-write requires --write-sid and --temp-write-sid together".into(),
        ));
    }
    if let (Some(write_sid), Some(temp_sid)) = (&request.write_sid, &request.temp_write_sid) {
        if write_sid != &workspace_write_sid(&request.workspace) {
            return Err(WindowsAclRunnerError(
                "--write-sid does not match --workspace".into(),
            ));
        }
        if temp_sid != &temp_write_sid(&request.temp) {
            return Err(WindowsAclRunnerError(
                "--temp-write-sid does not match --temp".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(entry: WindowsAclRunnerEntry) -> WindowsAclRunnerInvocation {
        WindowsAclRunnerInvocation {
            node: PathBuf::from("/usr/bin/node"),
            entry,
        }
    }

    #[test]
    fn source_prefix_matches_the_typescript_tsx_launch() {
        let invocation = invocation(WindowsAclRunnerEntry::SourceTs(PathBuf::from(
            "/repo/packages/sandbox/sandbox-windows-acl/src/runner.ts",
        )));
        assert_eq!(
            windows_acl_runner_prefix(&invocation),
            [
                "/usr/bin/node",
                "--import",
                "tsx/esm",
                "/repo/packages/sandbox/sandbox-windows-acl/src/runner.ts"
            ]
        );
    }

    #[test]
    fn built_prefix_is_node_plus_runner_js() {
        let invocation = invocation(WindowsAclRunnerEntry::BuiltJs(PathBuf::from(
            "/repo/packages/sandbox/sandbox-windows-acl/lib/runner.js",
        )));
        assert_eq!(
            windows_acl_runner_prefix(&invocation),
            [
                "/usr/bin/node",
                "/repo/packages/sandbox/sandbox-windows-acl/lib/runner.js"
            ]
        );
    }

    #[test]
    fn agentless_workspace_write_omits_sid_flags() {
        let request = WindowsAclRunnerRequest {
            workspace: r"C:\Users\agent\repo".into(),
            temp: r"C:\Users\agent\AppData\Local\Temp".into(),
            mode: WindowsAclRunnerMode::WorkspaceWrite,
            write_sid: None,
            temp_write_sid: None,
            command: vec!["pwsh.exe".into(), "-Command".into(), "echo hi".into()],
        };
        assert_eq!(
            windows_acl_runner_flags(&request).unwrap(),
            [
                "--workspace",
                r"C:\Users\agent\repo",
                "--temp",
                r"C:\Users\agent\AppData\Local\Temp",
                "--mode",
                "workspace-write",
                "--",
                "pwsh.exe",
                "-Command",
                "echo hi"
            ]
        );
    }

    #[test]
    fn seam_managed_workspace_write_includes_matching_sids() {
        let workspace = r"C:\Users\agent\repo";
        let temp = r"C:\Users\agent\AppData\Local\Temp\dsh-abc123";
        let request = WindowsAclRunnerRequest {
            workspace: workspace.into(),
            temp: temp.into(),
            mode: WindowsAclRunnerMode::WorkspaceWrite,
            write_sid: Some(workspace_write_sid(workspace)),
            temp_write_sid: Some(temp_write_sid(temp)),
            command: vec!["pwsh.exe".into()],
        };
        let flags = windows_acl_runner_flags(&request).unwrap();
        assert!(flags
            .windows(2)
            .any(|pair| { pair == ["--write-sid", "S-1-4-907248133-152761708"] }));
        assert!(flags
            .windows(2)
            .any(|pair| pair == ["--temp-write-sid", "S-1-4-174242848-241453763-1"]));
        let invocation = invocation(WindowsAclRunnerEntry::BuiltJs(PathBuf::from("runner.js")));
        let argv = windows_acl_runner_argv(&invocation, &request).unwrap();
        let parsed = parse_windows_acl_runner_args(&argv[2..]).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn read_only_rejects_sid_flags() {
        let err = windows_acl_runner_flags(&WindowsAclRunnerRequest {
            workspace: r"C:\repo".into(),
            temp: r"C:\Temp".into(),
            mode: WindowsAclRunnerMode::ReadOnly,
            write_sid: Some(workspace_write_sid(r"C:\repo")),
            temp_write_sid: Some(temp_write_sid(r"C:\Temp")),
            command: vec!["pwsh.exe".into()],
        })
        .unwrap_err();
        assert_eq!(
            err.0,
            "read-only does not accept --write-sid or --temp-write-sid"
        );
        assert_eq!(
            err.signature_line(),
            "windows-acl-run: read-only does not accept --write-sid or --temp-write-sid"
        );
    }

    #[test]
    fn workspace_write_requires_sid_flags_together() {
        let err = windows_acl_runner_flags(&WindowsAclRunnerRequest {
            workspace: r"C:\repo".into(),
            temp: r"C:\Temp".into(),
            mode: WindowsAclRunnerMode::WorkspaceWrite,
            write_sid: Some(workspace_write_sid(r"C:\repo")),
            temp_write_sid: None,
            command: vec!["pwsh.exe".into()],
        })
        .unwrap_err();
        assert_eq!(
            err.0,
            "workspace-write requires --write-sid and --temp-write-sid together"
        );
    }

    #[test]
    fn seam_managed_sids_must_match_the_paths() {
        let err = windows_acl_runner_flags(&WindowsAclRunnerRequest {
            workspace: r"C:\Users\agent\repo".into(),
            temp: r"C:\Temp\dsh".into(),
            mode: WindowsAclRunnerMode::WorkspaceWrite,
            write_sid: Some("S-1-4-1-2".into()),
            temp_write_sid: Some(temp_write_sid(r"C:\Temp\dsh")),
            command: vec!["pwsh.exe".into()],
        })
        .unwrap_err();
        assert_eq!(err.0, "--write-sid does not match --workspace");
    }

    #[test]
    fn parse_rejects_unknown_mode_and_missing_command() {
        let err = parse_windows_acl_runner_args(&[
            "--workspace".into(),
            r"C:\repo".into(),
            "--temp".into(),
            r"C:\Temp".into(),
            "--mode".into(),
            "danger-full-access".into(),
            "--".into(),
            "pwsh.exe".into(),
        ])
        .unwrap_err();
        assert_eq!(err.0, "unknown mode: danger-full-access");
        let err = parse_windows_acl_runner_args(&[
            "--workspace".into(),
            r"C:\repo".into(),
            "--temp".into(),
            r"C:\Temp".into(),
            "--mode".into(),
            "read-only".into(),
            "--".into(),
        ])
        .unwrap_err();
        assert_eq!(err.0, "missing command after --");
    }

    #[test]
    fn resolves_the_monorepo_typescript_source_entry() {
        let package_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../packages/sandbox/sandbox-windows-acl");
        let entry = resolve_windows_acl_runner_entry(&package_root)
            .expect("TypeScript windows-acl package");
        match entry {
            WindowsAclRunnerEntry::BuiltJs(path) => {
                assert!(path.ends_with("lib/runner.js"));
            }
            WindowsAclRunnerEntry::SourceTs(path) => {
                assert!(path.ends_with("src/runner.ts"));
            }
        }
    }
}
