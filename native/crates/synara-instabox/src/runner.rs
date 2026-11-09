use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// What one CLI invocation produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawOutput {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `instabox` with arguments. A seam so tests can script the CLI.
pub trait Runner: Send + Sync {
    fn run(
        &self,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> impl Future<Output = std::io::Result<RawOutput>> + Send;

    /// The program and environment a long-running child (port-forward) uses.
    fn command(&self, args: &[String]) -> Command;
}

/// The real `instabox` binary.
#[derive(Debug, Clone)]
pub struct CliRunner {
    pub program: PathBuf,
    /// Passed as `INSTABOX_API_KEY` when set; otherwise the signed-in account.
    pub api_key: Option<String>,
}

impl Default for CliRunner {
    fn default() -> Self {
        Self {
            program: "instabox".into(),
            api_key: None,
        }
    }
}

impl Runner for CliRunner {
    async fn run(&self, args: Vec<String>, stdin: Option<Vec<u8>>) -> std::io::Result<RawOutput> {
        let mut command = self.command(&args);
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(&input).await?;
            drop(pipe);
        }
        let output = child.wait_with_output().await?;
        Ok(RawOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: strip_notices(&String::from_utf8_lossy(&output.stderr)),
        })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(args);
        if let Some(key) = &self.api_key {
            command.env("INSTABOX_API_KEY", key);
        }
        command
    }
}

/// Drops the CLI's "a newer version is available" lines, which it prints on
/// stderr ahead of real errors.
fn strip_notices(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| !line.contains("outdated"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_update_notices() {
        assert_eq!(
            strip_notices("instabox 0.8.219 is outdated; run instabox update\nreal error"),
            "real error"
        );
    }
}
