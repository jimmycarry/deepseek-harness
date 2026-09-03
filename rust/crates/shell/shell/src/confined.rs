//! Confine-and-stamp helpers shared by bash-sandbox and pwsh-sandbox.

use crate::{ShellChild, ShellChildExit, ShellError, ShellRunResult, ShellSandboxInfo, ShellSpec};
use dsh_sandbox::{
    classify_runner_failure, matches_signature, ConfinedArgv, RunnerFailureRule, SandboxEnforcement,
    SandboxError, SandboxExecutionPolicy, SandboxMode,
};
use std::sync::Mutex;

struct ProcessFacts {
    mode: SandboxMode,
    enforcement: SandboxEnforcement,
    denial_signatures: Vec<String>,
    runner_failure_rules: Vec<RunnerFailureRule>,
}

/// Background handle that stamps sandbox facts once the inner process settles.
pub struct ConfinedChild {
    inner: Box<dyn ShellChild>,
    facts: ProcessFacts,
    sandbox: Mutex<Option<ShellSandboxInfo>>,
}

impl ConfinedChild {
    /// Wrap a spawned confined process with the wrap's classification facts.
    pub fn new(inner: Box<dyn ShellChild>, confined: ConfinedArgv, mode: SandboxMode) -> Self {
        Self {
            inner,
            facts: ProcessFacts {
                mode,
                enforcement: confined.enforcement,
                denial_signatures: confined.denial_signatures,
                runner_failure_rules: confined.runner_failure_rules,
            },
            sandbox: Mutex::new(None),
        }
    }

    fn stamp(&self, exit: &ShellChildExit) {
        let mut slot = self.sandbox.lock().expect("sandbox facts");
        if slot.is_some() {
            return;
        }
        let stderr = self.inner.collected_stderr();
        let runner_failed =
            classify_runner_failure(exit.exit_code, &stderr, &self.facts.runner_failure_rules)
                .is_some();
        let denied = !runner_failed
            && matches_signature(exit.exit_code, &stderr, &self.facts.denial_signatures);
        *slot = Some(ShellSandboxInfo {
            mode: self.facts.mode,
            denied,
            enforcement: Some(self.facts.enforcement),
            runner_failed: runner_failed.then_some(true),
        });
    }
}

impl ShellChild for ConfinedChild {
    fn cancel(&self) {
        self.inner.cancel();
    }

    fn wait(&self) -> Result<ShellChildExit, ShellError> {
        let exit = self.inner.wait()?;
        self.stamp(&exit);
        Ok(exit)
    }

    fn read_output(&self) -> String {
        self.inner.read_output()
    }

    fn collected_stderr(&self) -> String {
        self.inner.collected_stderr()
    }

    fn sandbox_info(&self) -> Option<ShellSandboxInfo> {
        self.sandbox.lock().expect("sandbox facts").clone()
    }
}

/// Map a provider `SandboxError` onto the shell unavailable channel.
pub fn unavailable_error(error: SandboxError) -> ShellError {
    match error {
        SandboxError::Unavailable { message, .. } => ShellError::Unavailable(message),
    }
}

/// Foreground runner-failure as `SANDBOX_UNAVAILABLE` with the matched detail.
fn runner_unavailable(mode: SandboxMode, detail: &str) -> ShellError {
    unavailable_error(SandboxError::unavailable(mode.as_str(), Some(detail)))
}

/// Spawn cwd used for runner-spawn classification.
fn spawn_workdir(spec: &ShellSpec) -> String {
    spec.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into())
    })
}

/// Reclassify a spawn of confine argv[0] as sandbox-unavailable when the cwd is usable.
pub fn map_spawn(
    error: ShellError,
    confined: &ConfinedArgv,
    spec: &ShellSpec,
    mode: SandboxMode,
) -> ShellError {
    if error.is_runner_spawn_failure(confined.argv.first().map(String::as_str), &spawn_workdir(spec))
    {
        runner_unavailable(mode, &error.to_string())
    } else {
        error
    }
}

/// Stamp denial facts, or throw when runner-failure rules match.
pub fn stamp_foreground(
    mut result: ShellRunResult,
    policy: &SandboxExecutionPolicy,
    confined: &ConfinedArgv,
) -> Result<ShellRunResult, ShellError> {
    if let Some(failure) = classify_runner_failure(
        result.exit_code,
        &result.stderr.text,
        &confined.runner_failure_rules,
    ) {
        return Err(runner_unavailable(policy.mode, &failure.detail));
    }
    let denied = matches_signature(
        result.exit_code,
        &result.stderr.text,
        &confined.denial_signatures,
    );
    result.sandbox = Some(ShellSandboxInfo {
        mode: policy.mode,
        denied,
        enforcement: Some(confined.enforcement),
        runner_failed: None,
    });
    Ok(result)
}
