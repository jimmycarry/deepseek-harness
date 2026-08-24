//! Local process-tree subprocess provider.

use async_trait::async_trait;
use dsh_subprocess::{ProcessOutput, SpawnSpec, SubprocessError, SubprocessExecutor};
use std::collections::BTreeMap;
use tokio::process::Command;

/// Host process-tree backend.
pub struct LocalSubprocess;

/// Apply a trusted `DSH_*` overlay: drop inherited managed keys, then merge.
pub fn apply_dsh_env(command: &mut Command, dsh_env: &Option<BTreeMap<String, String>>) {
    let Some(overlay) = dsh_env else {
        return;
    };
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("DSH_"))
        .collect();
    env.extend(overlay.iter().map(|(key, value)| (key.clone(), value.clone())));
    command.env_clear();
    command.envs(env);
}

#[async_trait]
impl SubprocessExecutor for LocalSubprocess {
    async fn run(&self, spec: SpawnSpec) -> Result<ProcessOutput, SubprocessError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        apply_dsh_env(&mut command, &spec.dsh_env);
        let output = command
            .output()
            .await
            .map_err(|error| SubprocessError::Spawn(error.to_string()))?;
        Ok(ProcessOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            status: output.status.code().unwrap_or(-1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_is_real() {
        let out = LocalSubprocess
            .run(SpawnSpec {
                program: "echo".into(),
                args: vec!["ok".into()],
                cwd: None,
                dsh_env: None,
            })
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "ok");
        assert_eq!(out.status, 0);
    }
}
