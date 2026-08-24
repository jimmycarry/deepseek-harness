//! Local bash provider. Spawns through `ctx.subprocess`.

use async_trait::async_trait;
use dsh_shell::{ShellError, ShellExecutor, ShellSpec};
use dsh_subprocess::{resolve, SpawnRequest, SubprocessRuntime};
use std::sync::Arc;

/// Bash via `/bin/bash -lc`.
pub struct BashLocal {
    subprocess: Arc<SubprocessRuntime>,
}

impl BashLocal {
    /// Bind to the subprocess seam.
    pub fn new(subprocess: Arc<SubprocessRuntime>) -> Self {
        Self { subprocess }
    }
}

#[async_trait]
impl ShellExecutor for BashLocal {
    async fn run(&self, spec: ShellSpec) -> Result<String, ShellError> {
        let spawn = resolve(SpawnRequest {
            program: "/bin/bash".into(),
            args: vec!["-lc".into(), spec.command],
            cwd: spec.cwd,
            dsh_env: spec.dsh_env,
        });
        let output = self
            .subprocess
            .run(spawn)
            .await
            .map_err(|error| ShellError::Failed(error.to_string()))?;
        if output.status != 0 {
            return Err(ShellError::Failed(output.stderr));
        }
        Ok(output.stdout)
    }
}
