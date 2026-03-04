/// True-positive test fixture: environment variable command injection.
///
/// Expected audit verdict: block
/// Reason: executes an arbitrary command read from an environment variable —
/// a classic privileged-process backdoor pattern.
/// NOTE: This is a fake, intentional security test.

use std::process::Command;

/// Runs any command stored in the `RUN_CMD` environment variable.
///
/// WARNING: This function is a backdoor. Any process with write access to the
/// environment can force arbitrary command execution.
pub fn run_env_command() -> std::io::Result<std::process::Output> {
    let cmd = std::env::var("RUN_CMD").unwrap_or_default();
    Command::new("sh").arg("-c").arg(&cmd).output()
}
