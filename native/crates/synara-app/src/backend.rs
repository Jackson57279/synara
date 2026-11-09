//! Everything async lives here, on a Tokio runtime the UI never blocks on.
//!
//! The backend brings up a server (an existing URL, a supervised local
//! server, or an instabox sandbox), starts a `Session` against it, and turns
//! everything that happens into [`BackendEvent`]s on one channel. The window
//! reads stores through [`Backend::session`] and sends commands through the
//! fire-and-forget methods, whose failures come back as notices.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use synara_instabox::{Instabox, SynaraHost, SynaraHostSpec};
use synara_protocol::commands::{ClientCommand, InteractionMode, RuntimeMode};
use synara_protocol::ids::{ProjectId, ThreadId};
use synara_protocol::orchestration::ModelSelection;
use synara_protocol::server::ResolvedKeybinding;
use synara_session::{ConnectionState, Session, SessionConfig, SessionEvent};
use synara_supervisor::{ServerCommand, ServerSpec, Supervisor};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use url::Url;

/// Where the server comes from.
#[derive(Debug, Clone)]
pub enum Target {
    /// A server that is already running.
    Url { url: Url, token: Option<String> },
    /// Start `apps/server` from this checkout under `synara-supervisor`.
    Spawn { repo: PathBuf, home: PathBuf },
    /// Bring up the server inside this instabox sandbox and forward it.
    Instabox {
        sandbox: String,
        token: Option<String>,
    },
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Self::Url { url, .. } => url.to_string(),
            Self::Spawn { repo, .. } => format!("local server from {}", repo.display()),
            Self::Instabox { sandbox, .. } => format!("instabox sandbox {sandbox}"),
        }
    }
}

#[derive(Debug)]
pub enum BackendEvent {
    /// Bring-up progress before the session exists.
    Progress(String),
    /// The session is running; stores are readable from now on.
    SessionStarted,
    Session(SessionEvent),
    /// A command failed or bring-up stopped; shown on the notice line.
    Notice(String),
    /// A thread this client created, so the window can select it.
    ThreadCreated(ThreadId),
    /// The server's resolved keybindings, read after each connect.
    Keybindings(Vec<ResolvedKeybinding>),
}

/// The host keeping the server alive, stopped on shutdown.
enum Host {
    None,
    Local(Supervisor),
    Instabox(Box<SynaraHost>),
}

pub struct Backend {
    runtime: Runtime,
    session: Arc<Mutex<Option<Session>>>,
    host: Arc<Mutex<Host>>,
    events: mpsc::UnboundedSender<BackendEvent>,
}

impl Backend {
    pub fn start(target: Target) -> std::io::Result<(Self, mpsc::UnboundedReceiver<BackendEvent>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("synara-backend")
            .enable_all()
            .build()?;
        let (events, events_rx) = mpsc::unbounded_channel();
        let backend = Self {
            runtime,
            session: Arc::new(Mutex::new(None)),
            host: Arc::new(Mutex::new(Host::None)),
            events,
        };
        backend.bring_up(target);
        Ok((backend, events_rx))
    }

    fn bring_up(&self, target: Target) {
        let events = self.events.clone();
        let session_slot = self.session.clone();
        let host_slot = self.host.clone();
        self.runtime.spawn(async move {
            let _ = events.send(BackendEvent::Progress(format!(
                "Starting {}…",
                target.describe()
            )));
            let (url, token) = match connect_target(target, &host_slot).await {
                Ok(origin) => origin,
                Err(error) => {
                    let _ = events.send(BackendEvent::Notice(error));
                    return;
                }
            };
            let mut config = SessionConfig::new(url);
            config.token = token;
            let (session, mut session_events) = Session::start(config);
            *session_slot.lock().expect("session lock") = Some(session);
            let _ = events.send(BackendEvent::SessionStarted);
            while let Some(event) = session_events.recv().await {
                if let SessionEvent::Connection(ConnectionState::Connected { .. }) = &event {
                    // Read from the slot: a clone held here would keep the
                    // session alive past `shutdown`.
                    let session = session_slot.lock().expect("session lock").clone();
                    if let Some(session) = session {
                        tokio::spawn(read_keybindings(session, events.clone()));
                    }
                }
                if events.send(BackendEvent::Session(event)).is_err() {
                    break;
                }
            }
        });
    }

    /// The running session, once bring-up finished.
    pub fn with_session<T>(&self, read: impl FnOnce(&Session) -> T) -> Option<T> {
        self.session
            .lock()
            .expect("session lock")
            .as_ref()
            .map(read)
    }

    pub fn open_thread(&self, thread_id: &ThreadId) {
        self.with_session(|session| session.open_thread(thread_id));
    }

    pub fn send_message(&self, thread_id: ThreadId, text: String) {
        self.run(move |session, events| async move {
            if let Err(error) = session
                .send_message(
                    &thread_id,
                    text,
                    RuntimeMode::default(),
                    InteractionMode::default(),
                )
                .await
            {
                let _ = events.send(BackendEvent::Notice(format!("Send failed: {error}")));
            }
        });
    }

    pub fn interrupt(&self, thread_id: ThreadId) {
        self.run(move |session, events| async move {
            if let Err(error) = session.interrupt(&thread_id).await {
                let _ = events.send(BackendEvent::Notice(format!("Stop failed: {error}")));
            }
        });
    }

    pub fn create_thread(&self, project_id: ProjectId, model_selection: ModelSelection) {
        self.run(move |session, events| async move {
            let command = ClientCommand::thread_create(project_id, "", model_selection);
            let ClientCommand::ThreadCreate { thread_id, .. } = &command else {
                unreachable!("thread_create builds a ThreadCreate");
            };
            let thread_id = thread_id.clone();
            match session.dispatch(&command).await {
                Ok(_) => {
                    let _ = events.send(BackendEvent::ThreadCreated(thread_id));
                }
                Err(error) => {
                    let _ = events.send(BackendEvent::Notice(format!(
                        "Could not create a thread: {error}"
                    )));
                }
            }
        });
    }

    pub fn reconnect(&self) {
        self.with_session(Session::reconnect);
    }

    /// Runs `task` on the runtime with a handle on the session. Without a
    /// session it reports that instead.
    fn run<F, Fut>(&self, task: F)
    where
        F: FnOnce(Session, mpsc::UnboundedSender<BackendEvent>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let events = self.events.clone();
        match self.with_session(Session::clone) {
            Some(session) => {
                self.runtime.spawn(task(session, events));
            }
            None => {
                let _ = events.send(BackendEvent::Notice("Not connected yet".into()));
            }
        }
    }

    /// Drops the session and stops the host. Blocks the caller up to the
    /// supervisor's own stop timeout plus a margin.
    pub fn shutdown(&self) {
        self.session.lock().expect("session lock").take();
        let host = std::mem::replace(&mut *self.host.lock().expect("host lock"), Host::None);
        self.runtime.block_on(async move {
            let stop = async move {
                match host {
                    Host::None => {}
                    Host::Local(supervisor) => supervisor.stop().await,
                    Host::Instabox(host) => host.close().await,
                }
            };
            if tokio::time::timeout(Duration::from_secs(45), stop)
                .await
                .is_err()
            {
                tracing::warn!("host did not stop in time");
            }
        });
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn read_keybindings(session: Session, events: mpsc::UnboundedSender<BackendEvent>) {
    match session.server_config().await {
        Ok(config) => {
            let _ = events.send(BackendEvent::Keybindings(config.keybindings));
        }
        Err(error) => tracing::warn!(%error, "could not read server config; no keybindings"),
    }
}

async fn connect_target(
    target: Target,
    host_slot: &Mutex<Host>,
) -> Result<(Url, Option<String>), String> {
    match target {
        Target::Url { url, token } => Ok((url, token)),
        Target::Spawn { repo, home } => {
            let mut spec = ServerSpec::new(ServerCommand::bun_source(&repo), &home);
            spec.log_file = Some(home.join("server.log"));
            let supervisor = Supervisor::start(spec)
                .await
                .map_err(|error| format!("Could not start the server: {error}"))?;
            let ready = supervisor.wait_ready(Duration::from_secs(180)).await;
            *host_slot.lock().expect("host lock") = Host::Local(supervisor);
            let ready = ready.map_err(|error| format!("Server did not start: {error}"))?;
            Ok((ready.url, Some(ready.auth_token)))
        }
        Target::Instabox { sandbox, token } => {
            let mut spec = SynaraHostSpec::new(&sandbox);
            spec.auth_token = token;
            let host = SynaraHost::bring_up(&Instabox::default(), &spec)
                .await
                .map_err(|error| format!("Could not bring up {sandbox}: {error}"))?;
            let origin = (host.url.clone(), Some(host.auth_token.clone()));
            *host_slot.lock().expect("host lock") = Host::Instabox(Box::new(host));
            Ok(origin)
        }
    }
}
