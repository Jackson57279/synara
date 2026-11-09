use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;

use crate::health::probe_health;
use crate::policy::{FailureResponse, RestartPolicy, SupervisionPolicy};
use crate::tail::{summarize_failure_output, OutputTail};

/// Environment the supervisor owns. Inherited values are replaced so a parent
/// Synara (or a shell export) cannot point the child at the wrong home or
/// leak its credentials.
const OWNED_ENV: &[&str] = &[
    "SYNARA_PORT",
    "SYNARA_HOST",
    "SYNARA_HOME",
    "SYNARA_AUTH_TOKEN",
    "SYNARA_NO_BROWSER",
    "SYNARA_DESKTOP_PARENT_STDIN",
];
/// Desktop-only values that must never reach a child the native client owns.
const STRIPPED_ENV: &[&str] = &["SYNARA_DESKTOP_SHUTDOWN_TOKEN", "ELECTRON_RUN_AS_NODE"];

/// How to launch the server process.
#[derive(Debug, Clone)]
pub struct ServerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl ServerCommand {
    /// `bun run src/index.ts` in a source checkout's `apps/server`.
    pub fn bun_source(repo_root: &Path) -> Self {
        Self {
            program: "bun".into(),
            args: vec!["run".into(), "src/index.ts".into()],
            cwd: Some(repo_root.join("apps/server")),
            env: vec![("SYNARA_MODE".into(), "web".into())],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub command: ServerCommand,
    /// `SYNARA_HOME`. Created if missing.
    pub home: PathBuf,
    pub host: String,
    /// Fixed port, or `None` to pick a free one once (kept across restarts so
    /// clients reconnect to the same URL).
    pub port: Option<u16>,
    /// Loopback token; generated when `None`.
    pub auth_token: Option<String>,
    /// Appends the server's output here as well as keeping the tail.
    pub log_file: Option<PathBuf>,
    /// A start that is not ready within this counts as a failure.
    pub ready_timeout: Option<Duration>,
    /// How long a stop waits after closing stdin before killing.
    pub stop_timeout: Duration,
    pub health_interval: Duration,
    /// Use a server that is already healthy on `port` instead of spawning.
    /// Requires `port` and `auth_token`. Adopted servers are not restarted.
    pub adopt_existing: bool,
    pub policy: RestartPolicy,
}

impl ServerSpec {
    pub fn new(command: ServerCommand, home: impl Into<PathBuf>) -> Self {
        Self {
            command,
            home: home.into(),
            host: "127.0.0.1".into(),
            port: None,
            auth_token: None,
            log_file: None,
            ready_timeout: Some(Duration::from_secs(120)),
            // The server bounds its own owner-lost shutdown at 20 s.
            stop_timeout: Duration::from_secs(25),
            health_interval: Duration::from_millis(100),
            adopt_existing: false,
            policy: RestartPolicy::default(),
        }
    }
}

/// `${SYNARA_HOME}/userdata/state.sqlite.migration-recovery.json`
/// (`migrationRecoveryMarkerPath` in `packages/shared`).
pub fn migration_recovery_marker(home: &Path) -> PathBuf {
    home.join("userdata")
        .join("state.sqlite.migration-recovery.json")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyServer {
    /// HTTP origin, such as `http://127.0.0.1:3773/`.
    pub url: Url,
    pub auth_token: String,
    pub pid: Option<u32>,
    pub adopted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Starting {
        attempt: u32,
    },
    Ready(ReadyServer),
    Restarting {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    /// Too many failed starts in a row; [`Supervisor::restart`] tries again.
    GaveUp {
        failures: u32,
        summary: String,
    },
    /// The database needs migration recovery; the server is not started.
    RecoveryPending {
        marker: PathBuf,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    #[error("server failed to start after {failures} attempts: {summary}")]
    GaveUp { failures: u32, summary: String },
    #[error("migration recovery pending ({})", marker.display())]
    RecoveryPending { marker: PathBuf },
    #[error("supervisor stopped")]
    Stopped,
    #[error("timed out waiting for the server")]
    Timeout,
}

enum Control {
    Restart,
    Stop,
}

/// Owns one server process. Dropping it stops the server (stdin EOF).
pub struct Supervisor {
    status: watch::Receiver<ServerStatus>,
    control: mpsc::UnboundedSender<Control>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    pub async fn start(spec: ServerSpec) -> std::io::Result<Self> {
        let token = spec
            .auth_token
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let http = reqwest::Client::builder()
            .build()
            .map_err(std::io::Error::other)?;
        let (status_tx, status) = watch::channel(ServerStatus::Starting { attempt: 1 });
        let (control, control_rx) = mpsc::unbounded_channel();

        if spec.adopt_existing {
            if let (Some(port), Some(token)) = (spec.port, spec.auth_token.clone()) {
                let url = origin(&spec.host, port)?;
                let healthy = probe_health(&http, &url, Duration::from_secs(1)).await;
                if healthy.is_some_and(|h| h.startup_ready) {
                    tracing::info!(%url, "adopting a running server");
                    status_tx.send_replace(ServerStatus::Ready(ReadyServer {
                        url,
                        auth_token: token,
                        pid: None,
                        adopted: true,
                    }));
                    let task = tokio::spawn(hold_adopted(status_tx, control_rx));
                    return Ok(Self::new(status, control, task));
                }
            }
        }

        let port = match spec.port {
            Some(port) => port,
            None => free_port(&spec.host)?,
        };
        let url = origin(&spec.host, port)?;
        std::fs::create_dir_all(&spec.home)?;
        let runner = Runner {
            policy: SupervisionPolicy::new(spec.policy),
            spec,
            port,
            url,
            token,
            http,
            status: status_tx,
            tail: Arc::new(Mutex::new(OutputTail::new())),
        };
        let task = tokio::spawn(runner.run(control_rx));
        Ok(Self::new(status, control, task))
    }

    fn new(
        status: watch::Receiver<ServerStatus>,
        control: mpsc::UnboundedSender<Control>,
        task: JoinHandle<()>,
    ) -> Self {
        Self {
            status,
            control,
            task: Mutex::new(Some(task)),
        }
    }

    pub fn status(&self) -> ServerStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<ServerStatus> {
        self.status.clone()
    }

    /// Resolves once the server is ready, or with why it will not be.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<ReadyServer, WaitError> {
        let mut rx = self.status.clone();
        let wait = async move {
            loop {
                let settled = match &*rx.borrow_and_update() {
                    ServerStatus::Ready(ready) => Some(Ok(ready.clone())),
                    ServerStatus::GaveUp { failures, summary } => Some(Err(WaitError::GaveUp {
                        failures: *failures,
                        summary: summary.clone(),
                    })),
                    ServerStatus::RecoveryPending { marker } => {
                        Some(Err(WaitError::RecoveryPending {
                            marker: marker.clone(),
                        }))
                    }
                    ServerStatus::Stopped => Some(Err(WaitError::Stopped)),
                    ServerStatus::Starting { .. } | ServerStatus::Restarting { .. } => None,
                };
                if let Some(result) = settled {
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err(WaitError::Stopped);
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or(Err(WaitError::Timeout))
    }

    /// A deliberate restart: clears the failure breaker and starts again,
    /// including after a give-up or once recovery cleared its marker.
    pub fn restart(&self) {
        let _ = self.control.send(Control::Restart);
    }

    /// Stops the server gracefully and waits for it to exit.
    pub async fn stop(&self) {
        let _ = self.control.send(Control::Stop);
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // The task finishes the graceful stop on its own; if the runtime goes
        // away first, `kill_on_drop` still takes the child down.
        let _ = self.control.send(Control::Stop);
    }
}

fn origin(host: &str, port: u16) -> std::io::Result<Url> {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Url::parse(&format!("http://{host}:{port}/")).map_err(std::io::Error::other)
}

fn free_port(host: &str) -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind((host, 0))?;
    Ok(listener.local_addr()?.port())
}

async fn hold_adopted(
    status: watch::Sender<ServerStatus>,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    // Not ours to restart or stop; just report the end of supervision.
    let _ = control.recv().await;
    status.send_replace(ServerStatus::Stopped);
}

enum Outcome {
    Stop,
    Restart,
    Failed(String),
}

struct Runner {
    spec: ServerSpec,
    port: u16,
    url: Url,
    token: String,
    http: reqwest::Client,
    status: watch::Sender<ServerStatus>,
    policy: SupervisionPolicy,
    tail: Arc<Mutex<OutputTail>>,
}

impl Runner {
    fn set(&self, status: ServerStatus) {
        tracing::debug!(?status, "server status");
        self.status.send_replace(status);
    }

    async fn run(mut self, mut control: mpsc::UnboundedReceiver<Control>) {
        loop {
            let marker = migration_recovery_marker(&self.spec.home);
            if marker.exists() {
                self.set(ServerStatus::RecoveryPending { marker });
                match control.recv().await {
                    None | Some(Control::Stop) => break,
                    Some(Control::Restart) => {
                        self.policy.reset();
                        continue;
                    }
                }
            }

            self.set(ServerStatus::Starting {
                attempt: self.policy.consecutive_failures() + 1,
            });
            self.tail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            let reason = match self.spawn() {
                Ok((mut child, owner)) => {
                    let pid = child.id();
                    let outcome = self.watch(&mut child, &mut control).await;
                    stop_child(&mut child, pid, owner, self.spec.stop_timeout).await;
                    match outcome {
                        Outcome::Stop => break,
                        Outcome::Restart => {
                            self.policy.reset();
                            continue;
                        }
                        Outcome::Failed(reason) => reason,
                    }
                }
                Err(error) => format!(
                    "could not start {}: {error}",
                    self.spec.command.program.display()
                ),
            };

            tracing::warn!(%reason, "server start failed");
            match self.policy.record_failure() {
                FailureResponse::Retry { delay, attempt } => {
                    self.set(ServerStatus::Restarting {
                        attempt,
                        delay,
                        reason,
                    });
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        command = control.recv() => match command {
                            None | Some(Control::Stop) => break,
                            Some(Control::Restart) => self.policy.reset(),
                        },
                    }
                }
                FailureResponse::GiveUp { failures } => {
                    let output = self
                        .tail
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .read();
                    let summary = summarize_failure_output(&output);
                    self.set(ServerStatus::GaveUp {
                        failures,
                        summary: if summary.is_empty() { reason } else { summary },
                    });
                    match control.recv().await {
                        None | Some(Control::Stop) => break,
                        Some(Control::Restart) => self.policy.reset(),
                    }
                }
            }
        }
        self.set(ServerStatus::Stopped);
    }

    /// Returns the child and its stdin. Stdin is held outside the `Child`
    /// because `Child::wait` closes it, which the server reads as its owner
    /// exiting.
    fn spawn(&self) -> std::io::Result<(Child, Option<ChildStdin>)> {
        let spec = &self.spec;
        let mut command = Command::new(&spec.command.program);
        command.args(&spec.command.args);
        if let Some(cwd) = &spec.command.cwd {
            command.current_dir(cwd);
        }
        for key in OWNED_ENV.iter().chain(STRIPPED_ENV) {
            command.env_remove(key);
        }
        command
            .envs(spec.command.env.iter().map(|(k, v)| (k, v)))
            .env("SYNARA_PORT", self.port.to_string())
            .env("SYNARA_HOST", &spec.host)
            .env("SYNARA_HOME", &spec.home)
            .env("SYNARA_AUTH_TOKEN", &self.token)
            .env("SYNARA_NO_BROWSER", "1")
            // The server exits when this pipe reaches EOF, including when this
            // process crashes.
            .env("SYNARA_DESKTOP_PARENT_STDIN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a forced stop also reaches the provider
        // processes the server started instead of orphaning them.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        tracing::info!(pid = child.id(), port = self.port, "server spawned");
        let owner = child.stdin.take();

        let (chunks, chunks_rx) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, chunks.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, chunks));
        }
        tokio::spawn(collect(
            chunks_rx,
            self.tail.clone(),
            self.spec.log_file.clone(),
        ));
        Ok((child, owner))
    }

    async fn watch(
        &mut self,
        child: &mut Child,
        control: &mut mpsc::UnboundedReceiver<Control>,
    ) -> Outcome {
        let deadline = self.spec.ready_timeout.map(|t| Instant::now() + t);
        let mut ready = false;
        let mut tick = tokio::time::interval(self.spec.health_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                status = child.wait() => {
                    let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
                    return Outcome::Failed(if ready {
                        format!("server exited ({status})")
                    } else {
                        format!("server exited before it was ready ({status})")
                    });
                }
                command = control.recv() => return match command {
                    None | Some(Control::Stop) => Outcome::Stop,
                    Some(Control::Restart) => Outcome::Restart,
                },
                _ = tick.tick(), if !ready => {
                    let health = probe_health(&self.http, &self.url, Duration::from_secs(1)).await;
                    if health.is_some_and(|h| h.startup_ready) {
                        ready = true;
                        self.policy.record_ready();
                        self.set(ServerStatus::Ready(ReadyServer {
                            url: self.url.clone(),
                            auth_token: self.token.clone(),
                            pid: child.id(),
                            adopted: false,
                        }));
                    } else if deadline.is_some_and(|d| Instant::now() >= d) {
                        return Outcome::Failed("server did not become ready in time".into());
                    }
                }
            }
        }
    }
}

/// Grace period between SIGTERM and SIGKILL for a forced stop.
const TERM_GRACE: Duration = Duration::from_secs(3);

/// Closes stdin so the server shuts itself down. If that takes longer than
/// `timeout`, terminates the server's whole process group, then kills it.
async fn stop_child(
    child: &mut Child,
    pid: Option<u32>,
    owner: Option<ChildStdin>,
    timeout: Duration,
) {
    drop(owner);
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    if tokio::time::timeout(timeout, child.wait()).await.is_ok() {
        return;
    }
    tracing::warn!(
        ?pid,
        ?timeout,
        "server did not exit after stdin EOF; terminating its process group"
    );
    #[cfg(unix)]
    if let Some(pid) = pid {
        signal_group(pid, Signal::SIGTERM);
        let exited = tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok();
        // Stragglers in the group get SIGKILL even when the leader exited.
        signal_group(pid, Signal::SIGKILL);
        if exited {
            return;
        }
    }
    let _ = child.kill().await;
}

#[cfg(unix)]
use nix::sys::signal::Signal;

#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let Ok(pgid) = i32::try_from(pid) else {
        return;
    };
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => {
            tracing::warn!(pgid, ?signal, %error, "signalling the server process group failed");
        }
    }
}

async fn pump(mut source: impl AsyncRead + Unpin, sink: mpsc::UnboundedSender<String>) {
    let mut buf = vec![0u8; 8192];
    loop {
        match source.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if sink
                    .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn collect(
    mut chunks: mpsc::UnboundedReceiver<String>,
    tail: Arc<Mutex<OutputTail>>,
    log_file: Option<PathBuf>,
) {
    let mut log = match log_file {
        Some(path) => tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .inspect_err(
                |error| tracing::warn!(path = %path.display(), %error, "server log unavailable"),
            )
            .ok(),
        None => None,
    };
    while let Some(chunk) = chunks.recv().await {
        tail.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(&chunk);
        if let Some(file) = &mut log {
            if file.write_all(chunk.as_bytes()).await.is_err() {
                log = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in server: serves `/health` on `SYNARA_PORT` and exits when
    /// stdin closes, like `withDesktopParentLifetime`.
    const FAKE_SERVER: &str = r#"
import http.server, json, os, sys, threading
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"status": "ok", "startupReady": True,
                           "home": os.environ["SYNARA_HOME"]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
server = http.server.HTTPServer(("127.0.0.1", int(os.environ["SYNARA_PORT"])), H)
threading.Thread(target=server.serve_forever, daemon=True).start()
assert os.environ["SYNARA_DESKTOP_PARENT_STDIN"] == "1"
print("listening", flush=True)
sys.stdin.read()
print("owner gone", flush=True)
"#;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synara-supervisor-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn has_python() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn fast(spec: &mut ServerSpec) {
        spec.policy = RestartPolicy {
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            max_consecutive_failures: 3,
        };
        spec.health_interval = Duration::from_millis(20);
    }

    #[tokio::test]
    async fn starts_reports_ready_and_stops_on_stdin_eof() {
        if !has_python() {
            eprintln!("skipping: python3 not found");
            return;
        }
        let home = temp_home("ready");
        let log = home.join("server.log");
        let command = ServerCommand {
            program: "python3".into(),
            args: vec!["-c".into(), FAKE_SERVER.into()],
            cwd: None,
            env: Vec::new(),
        };
        let mut spec = ServerSpec::new(command, &home);
        spec.log_file = Some(log.clone());
        spec.stop_timeout = Duration::from_secs(30);
        fast(&mut spec);
        let supervisor = Supervisor::start(spec).await.unwrap();
        let ready = supervisor
            .wait_ready(Duration::from_secs(20))
            .await
            .unwrap();
        assert!(!ready.adopted);
        assert_eq!(ready.auth_token.len(), 32);

        let http = reqwest::Client::new();
        let health = probe_health(&http, &ready.url, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(health.startup_ready);

        // Adopting the same server does not spawn a second one.
        let mut adopt =
            ServerSpec::new(ServerCommand::bun_source(Path::new("/nonexistent")), &home);
        adopt.port = ready.url.port();
        adopt.auth_token = Some(ready.auth_token.clone());
        adopt.adopt_existing = true;
        let adopted = Supervisor::start(adopt).await.unwrap();
        assert!(
            adopted
                .wait_ready(Duration::from_secs(1))
                .await
                .unwrap()
                .adopted
        );
        adopted.stop().await;
        assert!(probe_health(&http, &ready.url, Duration::from_secs(2))
            .await
            .is_some());

        // Stop must finish through stdin EOF, well before the kill timeout.
        let started = Instant::now();
        supervisor.stop().await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(supervisor.status(), ServerStatus::Stopped);
        assert!(probe_health(&http, &ready.url, Duration::from_millis(500))
            .await
            .is_none());
        let logged = tokio::fs::read_to_string(&log).await.unwrap();
        assert!(logged.contains("listening"), "{logged}");
        let _ = std::fs::remove_dir_all(home);
    }

    /// Alive and not a zombie.
    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        let exists =
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None::<Signal>).is_ok();
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit(") ")
                .next()
                .is_some_and(|rest| rest.starts_with('Z'))
        });
        exists && !zombie
    }

    /// A server that ignores stdin EOF and SIGTERM and has a child of its own:
    /// a forced stop must take down the whole process group.
    #[cfg(unix)]
    #[tokio::test]
    async fn forced_stop_reaches_the_whole_process_group() {
        if !has_python() {
            eprintln!("skipping: python3 not found");
            return;
        }
        let home = temp_home("group");
        let pid_file = home.join("grandchild.pid");
        let server = home.join("stubborn.py");
        std::fs::write(
            &server,
            FAKE_SERVER.replace(
                "sys.stdin.read()",
                "sys.stdin.read(); import time; time.sleep(300)",
            ),
        )
        .unwrap();
        let script = format!(
            "sleep 300 & echo $! > '{}'; trap '' TERM; exec python3 '{}'",
            pid_file.display(),
            server.display()
        );
        let command = ServerCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script],
            cwd: None,
            env: Vec::new(),
        };
        let mut spec = ServerSpec::new(command, &home);
        spec.stop_timeout = Duration::from_millis(300);
        fast(&mut spec);
        let supervisor = Supervisor::start(spec).await.unwrap();
        supervisor
            .wait_ready(Duration::from_secs(20))
            .await
            .unwrap();
        let grandchild: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(grandchild));
        supervisor.stop().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived the stop"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn crash_loop_gives_up_with_a_summary_and_restart_retries() {
        let home = temp_home("crash");
        let command = ServerCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo booting; echo 'Error: listen EADDRINUSE' >&2; exit 3".into(),
            ],
            cwd: None,
            env: Vec::new(),
        };
        let mut spec = ServerSpec::new(command, &home);
        fast(&mut spec);
        let supervisor = Supervisor::start(spec).await.unwrap();
        let mut statuses = supervisor.subscribe();
        let error = supervisor
            .wait_ready(Duration::from_secs(20))
            .await
            .unwrap_err();
        let WaitError::GaveUp { failures, summary } = error else {
            panic!("expected a give-up, got {error:?}");
        };
        assert_eq!(failures, 3);
        assert!(summary.contains("EADDRINUSE"), "{summary}");

        statuses.borrow_and_update();
        supervisor.restart();
        statuses.changed().await.unwrap();
        assert_eq!(*statuses.borrow(), ServerStatus::Starting { attempt: 1 });
        supervisor.stop().await;
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn missing_program_counts_as_a_failed_start() {
        let home = temp_home("missing");
        let command = ServerCommand {
            program: "/nonexistent/synara-server".into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        };
        let mut spec = ServerSpec::new(command, &home);
        fast(&mut spec);
        let supervisor = Supervisor::start(spec).await.unwrap();
        let error = supervisor
            .wait_ready(Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, WaitError::GaveUp { summary, .. } if summary.contains("could not start")),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn migration_marker_blocks_the_start() {
        let home = temp_home("marker");
        let marker = migration_recovery_marker(&home);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "{}").unwrap();
        let command = ServerCommand {
            program: "/nonexistent/synara-server".into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        };
        let supervisor = Supervisor::start(ServerSpec::new(command, &home))
            .await
            .unwrap();
        assert_eq!(
            supervisor.wait_ready(Duration::from_secs(5)).await,
            Err(WaitError::RecoveryPending { marker })
        );
        supervisor.stop().await;
        assert_eq!(supervisor.status(), ServerStatus::Stopped);
        let _ = std::fs::remove_dir_all(home);
    }
}
