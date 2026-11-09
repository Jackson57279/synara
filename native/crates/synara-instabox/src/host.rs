use std::process::Stdio;
use std::time::Duration;

use tokio::process::Child;
use tokio::time::Instant;
use url::Url;

use crate::cli::{ExecOptions, Instabox, InstaboxError};
use crate::runner::Runner;

/// Where the server lives inside the sandbox.
#[derive(Debug, Clone)]
pub struct SynaraHostSpec {
    pub sandbox: String,
    /// Checkout inside the sandbox, relative to its home or absolute.
    pub repo_dir: String,
    /// Port the server binds on the sandbox's loopback.
    pub remote_port: u16,
    /// `SYNARA_HOME` inside the sandbox; never a real `~/.synara`.
    pub server_home: String,
    /// Loopback token. When `None`, the sandbox keeps one in
    /// `~/.synara-native-token` (created on first use) and it is read back.
    pub auth_token: Option<String>,
}

impl SynaraHostSpec {
    pub fn new(sandbox: impl Into<String>) -> Self {
        Self {
            sandbox: sandbox.into(),
            repo_dir: "synara".into(),
            remote_port: 3773,
            server_home: ".synara-native-host".into(),
            auth_token: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Instabox(#[from] InstaboxError),
    #[error("server in {sandbox} did not start: {detail}")]
    ServerStart { sandbox: String, detail: String },
    #[error("port-forward to {sandbox} failed: {detail}")]
    Forward { sandbox: String, detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A sandbox with a ready server, reachable on a local port.
pub struct SynaraHost {
    pub sandbox: String,
    /// Local HTTP origin of the forwarded server, e.g. `http://127.0.0.1:41234/`.
    pub url: Url,
    pub auth_token: String,
    forward: PortForward,
}

impl std::fmt::Debug for SynaraHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token stays out of logs.
        f.debug_struct("SynaraHost")
            .field("sandbox", &self.sandbox)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl SynaraHost {
    /// Starts (or resumes) the sandbox, starts its server if it is not
    /// already healthy, and forwards it to a free local port.
    pub async fn bring_up<R: Runner>(
        instabox: &Instabox<R>,
        spec: &SynaraHostSpec,
    ) -> Result<Self, HostError> {
        instabox.ensure_running(&spec.sandbox).await?;
        let auth_token = start_server(instabox, spec).await?;
        let forward = PortForward::open(instabox.runner(), &spec.sandbox, spec.remote_port).await?;
        let url = forward.url();
        tracing::info!(sandbox = %spec.sandbox, %url, "instabox host ready");
        Ok(Self {
            sandbox: spec.sandbox.clone(),
            url,
            auth_token,
            forward,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.forward.local_port
    }

    /// Closes the forward. The sandbox and its server keep running, to be
    /// paused by instabox's idle timer.
    pub async fn close(self) {
        self.forward.close().await;
    }
}

/// Shell run inside the sandbox: settle the token, then hand over to
/// `native/scripts/box-server.sh`, which is a no-op when the server is up.
/// Prints `token=<value>` as its last stdout line.
pub(crate) fn server_script(spec: &SynaraHostSpec) -> String {
    let token_setup = match &spec.auth_token {
        Some(token) => format!(
            "tok={}; printf %s \"$tok\" > ~/.synara-native-token; chmod 600 ~/.synara-native-token",
            shell_quote(token)
        ),
        None => "if [ ! -s ~/.synara-native-token ]; then (umask 077; head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \\n' > ~/.synara-native-token); fi; tok=$(cat ~/.synara-native-token)".into(),
    };
    format!(
        "set -e; {token_setup}; cd {repo}; SYNARA_PORT={port} SYNARA_NATIVE_HOME={home} SYNARA_AUTH_TOKEN=\"$tok\" native/scripts/box-server.sh >&2; echo \"token=$tok\"",
        repo = shell_quote(&spec.repo_dir),
        port = spec.remote_port,
        home = shell_quote(&spec.server_home),
    )
}

async fn start_server<R: Runner>(
    instabox: &Instabox<R>,
    spec: &SynaraHostSpec,
) -> Result<String, HostError> {
    let options = ExecOptions {
        timeout: Some(Duration::from_secs(240)),
        ..ExecOptions::default()
    };
    let result = instabox
        .exec(&spec.sandbox, &server_script(spec), &options)
        .await?;
    let token = result
        .stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("token="))
        .map(str::to_owned)
        .filter(|token| !token.is_empty());
    match token {
        Some(token) if result.success() => Ok(token),
        _ => Err(HostError::ServerStart {
            sandbox: spec.sandbox.clone(),
            detail: last_lines(&result.stderr, 12),
        }),
    }
}

/// A running `instabox port-forward`. Killed on drop.
pub struct PortForward {
    child: Child,
    pub local_port: u16,
}

impl PortForward {
    /// Forwards a free `127.0.0.1` port to `remote_port` and waits until the
    /// server answers `/health` through it.
    pub async fn open<R: Runner>(
        runner: &R,
        sandbox: &str,
        remote_port: u16,
    ) -> Result<Self, HostError> {
        let local_port = std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port();
        let mut command = runner.command(&[
            "port-forward".into(),
            sandbox.into(),
            format!("{local_port}:{remote_port}"),
        ]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut forward = Self {
            child: command.spawn()?,
            local_port,
        };
        let http = reqwest::Client::new();
        let health = forward.url().join("/health").expect("static path");
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(status) = forward.child.try_wait()? {
                let stderr = match forward.child.stderr.take() {
                    Some(mut pipe) => {
                        let mut text = String::new();
                        let _ = tokio::io::AsyncReadExt::read_to_string(&mut pipe, &mut text).await;
                        text
                    }
                    None => String::new(),
                };
                return Err(HostError::Forward {
                    sandbox: sandbox.into(),
                    detail: format!("exited ({status}): {}", last_lines(&stderr, 6)),
                });
            }
            let ready = http
                .get(health.clone())
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .ok()
                .filter(|response| response.status().is_success());
            if let Some(response) = ready {
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                if body["startupReady"] == true {
                    return Ok(forward);
                }
            }
            if Instant::now() >= deadline {
                return Err(HostError::Forward {
                    sandbox: sandbox.into(),
                    detail: "server not reachable through the forward".into(),
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    pub fn url(&self) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}/", self.local_port)).expect("valid origin")
    }

    pub async fn close(mut self) {
        let _ = self.child.kill().await;
    }
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./~".contains(c))
        && !value.starts_with('~')
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tests::Scripted;

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(shell_quote("synara"), "synara");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn script_uses_the_given_token_or_keeps_one() {
        let mut spec = SynaraHostSpec::new("dev");
        let generated = server_script(&spec);
        assert!(generated.contains("/dev/urandom"));
        assert!(generated.contains("SYNARA_PORT=3773"));
        assert!(generated.ends_with("echo \"token=$tok\""));
        spec.auth_token = Some("t'ok".into());
        assert!(server_script(&spec).starts_with(r"set -e; tok='t'\''ok';"));
    }

    #[tokio::test]
    async fn bring_up_reports_server_failures_with_its_output() {
        let ib = Instabox::new(
            Scripted::default()
                .reply(
                    0,
                    r#"{"id":"m","name":"dev","state":"running","runtime":{"ready":true}}"#,
                )
                .reply(
                    1,
                    r#"{"stdout":"","stderr":"server did not become ready\nError: EADDRINUSE\n","exitCode":1}"#,
                ),
        );
        let error = SynaraHost::bring_up(&ib, &SynaraHostSpec::new("dev"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, HostError::ServerStart { detail, .. } if detail.ends_with("EADDRINUSE")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn start_server_reads_the_token_back() {
        let ib = Instabox::new(Scripted::default().reply(
            0,
            r#"{"stdout":"token=abc123\n","stderr":"server already running on 3773\n","exitCode":0}"#,
        ));
        assert_eq!(
            start_server(&ib, &SynaraHostSpec::new("dev"))
                .await
                .unwrap(),
            "abc123"
        );
    }
}
