use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::runner::{CliRunner, RawOutput, Runner};
use crate::types::{CliError, CliErrorEnvelope, ExecResult, Sandbox, SecretList, SecretMeta};

/// `instabox exec` refuses timeouts above this.
pub const MAX_EXEC_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum InstaboxError {
    #[error("could not run instabox: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("instabox {command}: {} ({})", error.message, error.code)]
    Cli { command: String, error: CliError },
    #[error("instabox {command} exited with {code:?}: {stderr}")]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("instabox {command} printed unexpected JSON: {source}")]
    Decode {
        command: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid argument: {0}")]
    Invalid(String),
}

impl InstaboxError {
    pub fn cli_code(&self) -> Option<&str> {
        match self {
            Self::Cli { error, .. } => Some(&error.code),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub template: Option<String>,
    pub snapshot: Option<String>,
    pub vcpus: Option<u32>,
    pub mem_mib: Option<u32>,
    /// `--auto-suspend`, e.g. `30m` or `0`.
    pub auto_suspend: Option<String>,
    pub max_run_minutes: Option<u32>,
    /// Secret names to select at create time.
    pub secrets: Vec<String>,
    pub metadata: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Capped at [`MAX_EXEC_TIMEOUT`].
    pub timeout: Option<Duration>,
    pub stdin: Option<Vec<u8>>,
}

/// A typed handle on the `instabox` CLI.
#[derive(Debug, Clone)]
pub struct Instabox<R = CliRunner> {
    runner: R,
}

impl Default for Instabox<CliRunner> {
    fn default() -> Self {
        Self::new(CliRunner::default())
    }
}

impl<R: Runner> Instabox<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }

    pub async fn list(&self) -> Result<Vec<Sandbox>, InstaboxError> {
        self.json(args(["ls", "--json"])).await
    }

    /// `None` when no sandbox has that id or name.
    pub async fn get(&self, sandbox: &str) -> Result<Option<Sandbox>, InstaboxError> {
        match self.json(args(["get", sandbox, "--json"])).await {
            Ok(sandbox) => Ok(Some(sandbox)),
            Err(error) if error.cli_code() == Some("not_found") => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Creates a sandbox and waits until it runs.
    pub async fn create(
        &self,
        name: &str,
        options: &CreateOptions,
    ) -> Result<Sandbox, InstaboxError> {
        let mut argv = args(["create", "--name", name, "--json"]);
        let mut flag = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                argv.push(name.into());
                argv.push(value);
            }
        };
        flag("--template", options.template.clone());
        flag("--snapshot", options.snapshot.clone());
        flag("--vcpus", options.vcpus.map(|v| v.to_string()));
        flag("--mem-mib", options.mem_mib.map(|v| v.to_string()));
        flag("--auto-suspend", options.auto_suspend.clone());
        flag(
            "--max-run-minutes",
            options.max_run_minutes.map(|v| v.to_string()),
        );
        if !options.secrets.is_empty() {
            flag("--secrets", Some(options.secrets.join(",")));
        }
        for (key, value) in &options.metadata {
            flag("--meta", Some(format!("{key}={value}")));
        }
        self.json(argv).await
    }

    /// Starts a stopped sandbox or resumes a suspended one, waiting until it
    /// runs. A running sandbox is returned as is.
    pub async fn ensure_running(&self, sandbox: &str) -> Result<Sandbox, InstaboxError> {
        let current = self
            .get(sandbox)
            .await?
            .ok_or_else(|| InstaboxError::Invalid(format!("no sandbox named {sandbox}")))?;
        if current.is_ready() {
            return Ok(current);
        }
        let verb = if current.state == "suspended" {
            "resume"
        } else {
            "start"
        };
        self.json(args([verb, sandbox, "--json"])).await
    }

    pub async fn stop(&self, sandbox: &str) -> Result<(), InstaboxError> {
        self.ok(args(["stop", sandbox])).await
    }

    pub async fn suspend(&self, sandbox: &str) -> Result<(), InstaboxError> {
        self.ok(args(["suspend", sandbox])).await
    }

    /// Deletes the sandbox and its disks. Irreversible.
    pub async fn remove(&self, sandbox: &str) -> Result<(), InstaboxError> {
        self.ok(args(["rm", sandbox, "--yes"])).await
    }

    /// Runs `bash -lc <script>` as the sandbox user. A non-zero exit is a
    /// normal result, not an error; check [`ExecResult::success`].
    pub async fn exec(
        &self,
        sandbox: &str,
        script: &str,
        options: &ExecOptions,
    ) -> Result<ExecResult, InstaboxError> {
        let mut argv = args(["exec", sandbox, "--json"]);
        if let Some(cwd) = &options.cwd {
            argv.extend(args(["--cwd", cwd]));
        }
        for (key, value) in &options.env {
            if key.is_empty() || key.contains('=') {
                return Err(InstaboxError::Invalid(format!("env name {key:?}")));
            }
            argv.push("--env".into());
            argv.push(format!("{key}={value}"));
        }
        let timeout = options.timeout.unwrap_or(Duration::from_secs(60));
        argv.push("--timeout".into());
        argv.push(timeout.min(MAX_EXEC_TIMEOUT).as_secs().max(1).to_string());
        if options.stdin.is_some() {
            argv.push("--stdin".into());
        }
        argv.push("--".into());
        argv.push(script.into());
        let output = self.runner.run(argv.clone(), options.stdin.clone()).await?;
        // exec exits with the command's code, so judge by the JSON body. An
        // error envelope would also decode as an all-default result.
        if let Some(Ok(envelope)) = decode::<CliErrorEnvelope>(&output) {
            return Err(InstaboxError::Cli {
                command: command_name(&argv),
                error: envelope.error,
            });
        }
        match decode::<ExecResult>(&output) {
            Some(Ok(result)) => Ok(result),
            _ => Err(failure(&argv, &output)),
        }
    }

    pub async fn push(
        &self,
        sandbox: &str,
        local: &Path,
        remote: &str,
    ) -> Result<(), InstaboxError> {
        let local = local.to_string_lossy();
        self.ok(args(["push", sandbox, &local, remote, "-p"])).await
    }

    pub async fn pull(
        &self,
        sandbox: &str,
        remote: &str,
        local: &Path,
    ) -> Result<(), InstaboxError> {
        let local = local.to_string_lossy();
        self.ok(args(["pull", sandbox, remote, &local])).await
    }

    pub async fn secrets(&self) -> Result<Vec<SecretMeta>, InstaboxError> {
        let list: SecretList = self.json(args(["secrets", "ls", "--json"])).await?;
        Ok(list.items)
    }

    /// Replaces the sandbox's proxied Secret selection.
    pub async fn select_secrets(
        &self,
        sandbox: &str,
        names: &[String],
    ) -> Result<(), InstaboxError> {
        let selection = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(",")
        };
        self.ok(args([
            "secrets",
            "select",
            sandbox,
            "--secrets",
            &selection,
        ]))
        .await
    }

    async fn json<T: DeserializeOwned>(&self, argv: Vec<String>) -> Result<T, InstaboxError> {
        let output = self.runner.run(argv.clone(), None).await?;
        if output.code != Some(0) {
            return Err(failure(&argv, &output));
        }
        serde_json::from_str(&output.stdout).map_err(|source| InstaboxError::Decode {
            command: command_name(&argv),
            source,
        })
    }

    async fn ok(&self, argv: Vec<String>) -> Result<(), InstaboxError> {
        let output = self.runner.run(argv.clone(), None).await?;
        if output.code == Some(0) {
            Ok(())
        } else {
            Err(failure(&argv, &output))
        }
    }
}

fn args<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn command_name(argv: &[String]) -> String {
    argv.iter()
        .take_while(|a| !a.starts_with('-'))
        .take(2)
        .filter(|a| a.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode<T: DeserializeOwned>(output: &RawOutput) -> Option<serde_json::Result<T>> {
    let text = output.stdout.trim();
    (!text.is_empty()).then(|| serde_json::from_str(text))
}

fn failure(argv: &[String], output: &RawOutput) -> InstaboxError {
    let command = command_name(argv);
    for text in [&output.stdout, &output.stderr] {
        if let Ok(envelope) = serde_json::from_str::<CliErrorEnvelope>(text.trim()) {
            return InstaboxError::Cli {
                command,
                error: envelope.error,
            };
        }
    }
    InstaboxError::Failed {
        command,
        code: output.code,
        stderr: output.stderr.trim().to_owned(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use tokio::process::Command;

    use super::*;

    type Call = (Vec<String>, Option<Vec<u8>>);

    /// Replies with scripted outputs and records what it was asked to run.
    #[derive(Default)]
    pub struct Scripted {
        pub replies: Mutex<VecDeque<RawOutput>>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl Scripted {
        pub fn reply(self, code: i32, stdout: &str) -> Self {
            self.replies.lock().unwrap().push_back(RawOutput {
                code: Some(code),
                stdout: stdout.into(),
                stderr: String::new(),
            });
            self
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(a, _)| a.clone())
                .collect()
        }
    }

    impl Runner for Scripted {
        async fn run(
            &self,
            args: Vec<String>,
            stdin: Option<Vec<u8>>,
        ) -> std::io::Result<RawOutput> {
            self.calls.lock().unwrap().push((args, stdin));
            Ok(self.replies.lock().unwrap().pop_front().unwrap_or_default())
        }

        fn command(&self, args: &[String]) -> Command {
            let mut command = Command::new("true");
            command.args(args);
            command
        }
    }

    const RUNNING: &str = r#"{"id":"m_1","name":"dev","state":"running","runtime":{"ready":true}}"#;

    #[tokio::test]
    async fn get_maps_not_found_to_none() {
        let ib = Instabox::new(Scripted::default().reply(
            4,
            r#"{"error":{"code":"not_found","message":"no sandbox with ID or name \"x\""}}"#,
        ));
        assert!(ib.get("x").await.unwrap().is_none());

        let ib = Instabox::new(
            Scripted::default().reply(1, r#"{"error":{"code":"unauthorized","message":"no"}}"#),
        );
        let error = ib.get("x").await.unwrap_err();
        assert_eq!(error.cli_code(), Some("unauthorized"));
    }

    #[tokio::test]
    async fn create_passes_only_the_set_flags() {
        let ib = Instabox::new(Scripted::default().reply(0, RUNNING));
        let options = CreateOptions {
            vcpus: Some(4),
            mem_mib: Some(8192),
            secrets: vec!["OPENAI_API_KEY".into(), "ANTHROPIC_API_KEY".into()],
            metadata: vec![("synara".into(), "host".into())],
            ..CreateOptions::default()
        };
        let sandbox = ib.create("dev", &options).await.unwrap();
        assert!(sandbox.is_ready());
        assert_eq!(
            ib.runner().calls()[0],
            [
                "create",
                "--name",
                "dev",
                "--json",
                "--vcpus",
                "4",
                "--mem-mib",
                "8192",
                "--secrets",
                "OPENAI_API_KEY,ANTHROPIC_API_KEY",
                "--meta",
                "synara=host"
            ]
        );
    }

    #[tokio::test]
    async fn exec_keeps_nonzero_exits_as_results_and_caps_the_timeout() {
        let ib = Instabox::new(Scripted::default().reply(
            7,
            r#"{"stdout":"","stderr":"boom\n","exitCode":7,"timedOut":false,"durationMs":3}"#,
        ));
        let options = ExecOptions {
            cwd: Some("/data/home".into()),
            env: vec![("A".into(), "1 2".into())],
            timeout: Some(Duration::from_secs(3600)),
            stdin: Some(b"input".to_vec()),
        };
        let result = ib.exec("dev", "cat; exit 7", &options).await.unwrap();
        assert_eq!(result.exit_code, Some(7));
        let calls = ib.runner().calls.lock().unwrap();
        let (argv, stdin) = &calls[0];
        assert_eq!(
            argv,
            &[
                "exec",
                "dev",
                "--json",
                "--cwd",
                "/data/home",
                "--env",
                "A=1 2",
                "--timeout",
                "600",
                "--stdin",
                "--",
                "cat; exit 7"
            ]
        );
        assert_eq!(stdin.as_deref(), Some(&b"input"[..]));
    }

    #[tokio::test]
    async fn exec_rejects_bad_env_names_and_reports_cli_errors() {
        let ib = Instabox::new(Scripted::default());
        let options = ExecOptions {
            env: vec![("A=B".into(), "x".into())],
            ..ExecOptions::default()
        };
        assert!(matches!(
            ib.exec("dev", "true", &options).await,
            Err(InstaboxError::Invalid(_))
        ));

        let ib = Instabox::new(
            Scripted::default().reply(4, r#"{"error":{"code":"not_found","message":"gone"}}"#),
        );
        let error = ib
            .exec("dev", "true", &ExecOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.cli_code(), Some("not_found"));
        assert_eq!(error.to_string(), "instabox exec dev: gone (not_found)");
    }

    #[tokio::test]
    async fn ensure_running_resumes_a_suspended_sandbox() {
        let ib = Instabox::new(
            Scripted::default()
                .reply(0, r#"{"id":"m_1","name":"dev","state":"suspended"}"#)
                .reply(0, RUNNING),
        );
        assert!(ib.ensure_running("dev").await.unwrap().is_ready());
        assert_eq!(ib.runner().calls()[1], ["resume", "dev", "--json"]);
    }

    #[tokio::test]
    async fn secrets_list_and_select() {
        let ib = Instabox::new(
            Scripted::default()
                .reply(
                    0,
                    r#"{"items":[{"name":"OPENAI_API_KEY","hosts":["api.openai.com"],"createdAt":"x"}]}"#,
                )
                .reply(0, ""),
        );
        let secrets = ib.secrets().await.unwrap();
        assert_eq!(secrets[0].hosts, ["api.openai.com"]);
        ib.select_secrets("dev", &[]).await.unwrap();
        assert_eq!(
            ib.runner().calls()[1],
            ["secrets", "select", "dev", "--secrets", "none"]
        );
    }
}
