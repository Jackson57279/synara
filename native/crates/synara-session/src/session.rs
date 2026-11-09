use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;
use synara_protocol::commands::{
    settle_payload, ClientCommand, DispatchResult, InteractionMode, RuntimeMode, SettleResult,
};
use synara_protocol::generated::STREAM_LIMIT_THREAD;
use synara_protocol::ids::{MessageId, ThreadId};
use synara_protocol::negotiate::{ClientOffer, CompatibilityError, NegotiateResult};
use synara_protocol::orchestration::{ShellStreamItem, SubscribeThreadInput, ThreadStreamItem};
use synara_protocol::server::ServerConfig;
use synara_protocol::tags;
use synara_rpc::negotiate::negotiate;
use synara_rpc::{ClientConfig, ConnectError, Disconnect, RpcClient, RpcError};
use synara_sync::{ShellChange, ShellStore, ThreadChange, ThreadStatus, ThreadStore};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;

use crate::backoff::Backoff;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Server HTTP origin.
    pub url: Url,
    /// Loopback token (`?token=`).
    pub token: Option<String>,
    /// Bearer session for remote hosts.
    pub bearer: Option<String>,
    pub client_version: String,
    pub backoff: Backoff,
    pub rpc: ClientConfig,
}

impl SessionConfig {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            token: None,
            bearer: None,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            backoff: Backoff::default(),
            rpc: ClientConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Idle,
    Connecting {
        attempt: u32,
    },
    Connected {
        server_build: String,
        server_instance_id: String,
    },
    /// Lost or failed; retrying after `retry_in`.
    Waiting {
        reason: String,
        retry_in: Duration,
    },
    /// The server and this client cannot talk; only an update (or an explicit
    /// [`Session::reconnect`]) can fix it.
    Incompatible(CompatibilityError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Connection(ConnectionState),
    Shell(ShellChange),
    Thread {
        thread_id: ThreadId,
        change: ThreadChange,
    },
    /// A thread stream stopped for good (for example a stalled snapshot).
    ThreadFailed {
        thread_id: ThreadId,
        error: RpcError,
    },
}

#[derive(Debug)]
pub struct SessionState {
    pub connection: ConnectionState,
    pub shell: ShellStore,
    pub threads: HashMap<ThreadId, ThreadStore>,
    /// Threads with a live subscription, least recently opened first.
    open: Vec<ThreadId>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            connection: ConnectionState::Idle,
            shell: ShellStore::new(),
            threads: HashMap::new(),
            open: Vec::new(),
        }
    }

    pub fn open_threads(&self) -> &[ThreadId] {
        &self.open
    }

    /// Marks a thread open; returns the thread evicted to stay under the
    /// server's per-client thread-stream limit.
    fn open(&mut self, thread_id: &ThreadId) -> Option<ThreadId> {
        self.threads
            .entry(thread_id.clone())
            .or_insert_with(|| ThreadStore::new(thread_id.clone()));
        self.open.retain(|id| id != thread_id);
        self.open.push(thread_id.clone());
        (self.open.len() > STREAM_LIMIT_THREAD).then(|| self.open.remove(0))
    }

    fn close(&mut self, thread_id: &ThreadId) -> bool {
        let before = self.open.len();
        self.open.retain(|id| id != thread_id);
        before != self.open.len()
    }

    /// The server restarted: no cursor from the old instance is valid.
    fn forget_cursors(&mut self) {
        self.shell.reset();
        for store in self.threads.values_mut() {
            store.reset_cursor();
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SendError {
    #[error("not connected")]
    NotConnected,
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("{0}")]
    Rpc(RpcError),
    /// The command may or may not have run; the transcript will show which.
    #[error("delivery unknown: {0}")]
    Unknown(String),
}

enum Control {
    Open(ThreadId),
    Close(ThreadId),
    Reconnect,
}

struct Shared {
    state: Mutex<SessionState>,
    client: watch::Sender<Option<RpcClient>>,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    fn set_connection(&self, connection: ConnectionState) {
        self.lock().connection = connection.clone();
        self.emit(SessionEvent::Connection(connection));
    }

    fn current_client(&self) -> Option<RpcClient> {
        self.client.borrow().clone().filter(|c| !c.is_closed())
    }

    async fn wait_client(&self, timeout: Duration) -> Option<RpcClient> {
        let mut rx = self.client.subscribe();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(client) = rx.borrow_and_update().clone().filter(|c| !c.is_closed()) {
                return Some(client);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }
}

struct TaskGuard(JoinHandle<()>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Handle to a running session. Clones share it; the session stops when the
/// last clone is dropped.
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    control: mpsc::UnboundedSender<Control>,
    _task: Arc<TaskGuard>,
}

impl Session {
    /// Starts connecting in the background. Must run inside a Tokio runtime.
    pub fn start(config: SessionConfig) -> (Self, mpsc::UnboundedReceiver<SessionEvent>) {
        let (events, events_rx) = mpsc::unbounded_channel();
        let (control, control_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(SessionState::new()),
            client: watch::Sender::new(None),
            events,
        });
        let task = tokio::spawn(run(shared.clone(), config, control_rx));
        (
            Self {
                shared,
                control,
                _task: Arc::new(TaskGuard(task)),
            },
            events_rx,
        )
    }

    /// Read access for rendering. Do not hold across an await.
    pub fn state(&self) -> MutexGuard<'_, SessionState> {
        self.shared.lock()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.current_client().is_some()
    }

    /// Subscribes to a thread (or refreshes its recency). Opening more than the
    /// server's thread-stream limit closes the least recently opened one.
    pub fn open_thread(&self, thread_id: &ThreadId) {
        let evicted = self.shared.lock().open(thread_id);
        let _ = self.control.send(Control::Open(thread_id.clone()));
        if let Some(evicted) = evicted {
            let _ = self.control.send(Control::Close(evicted));
        }
    }

    pub fn close_thread(&self, thread_id: &ThreadId) {
        if self.shared.lock().close(thread_id) {
            let _ = self.control.send(Control::Close(thread_id.clone()));
        }
    }

    /// Drops the current connection (or skips the backoff wait) and connects again.
    pub fn reconnect(&self) {
        let _ = self.control.send(Control::Reconnect);
    }

    /// Dispatches a command and returns the accepted sequence.
    pub async fn dispatch(&self, command: &ClientCommand) -> Result<u64, SendError> {
        let client = self
            .shared
            .current_client()
            .ok_or(SendError::NotConnected)?;
        let result: DispatchResult = client
            .call(tags::ORCHESTRATION_DISPATCH_COMMAND, &command.to_value())
            .await
            .map_err(|error| match error {
                RpcError::Failure(failure) => SendError::Rejected(failure.to_string()),
                other => SendError::Rpc(other),
            })?;
        Ok(result.sequence)
    }

    /// Reads `server.getConfig` (keybindings and the rest of the config).
    pub async fn server_config(&self) -> Result<ServerConfig, SendError> {
        let client = self
            .shared
            .current_client()
            .ok_or(SendError::NotConnected)?;
        client
            .call(tags::SERVER_GET_CONFIG, &serde_json::json!({}))
            .await
            .map_err(SendError::Rpc)
    }

    /// Sends a user message. It shows in the transcript immediately and
    /// reconciles with the server's echo. A connection lost mid-dispatch is
    /// settled with `orchestration.settleTurnDispatch` after reconnecting, so
    /// the turn never starts twice.
    pub async fn send_message(
        &self,
        thread_id: &ThreadId,
        text: String,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> Result<u64, SendError> {
        let command = ClientCommand::turn_start(
            thread_id.clone(),
            text.clone(),
            runtime_mode,
            interaction_mode,
        );
        let ClientCommand::TurnStart {
            message,
            created_at,
            ..
        } = &command
        else {
            unreachable!("turn_start builds a TurnStart");
        };
        let message_id = message.message_id.clone();
        self.add_pending(thread_id, &message_id, text, created_at.clone());

        let Some(client) = self.shared.current_client() else {
            self.remove_pending(thread_id, &message_id);
            return Err(SendError::NotConnected);
        };
        match client
            .call::<_, DispatchResult>(tags::ORCHESTRATION_DISPATCH_COMMAND, &command.to_value())
            .await
        {
            Ok(result) => Ok(result.sequence),
            Err(RpcError::Disconnected(_)) => self.settle(thread_id, &message_id, &command).await,
            Err(error) => {
                self.remove_pending(thread_id, &message_id);
                Err(match error {
                    RpcError::Failure(failure) => SendError::Rejected(failure.to_string()),
                    other => SendError::Rpc(other),
                })
            }
        }
    }

    async fn settle(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        command: &ClientCommand,
    ) -> Result<u64, SendError> {
        let payload = settle_payload(command);
        let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(10));
        loop {
            let Some(client) = self.shared.wait_client(Duration::from_secs(120)).await else {
                return Err(SendError::Unknown(
                    "no connection to check delivery; check the conversation before sending again"
                        .into(),
                ));
            };
            match client
                .call::<_, SettleResult>(tags::ORCHESTRATION_SETTLE_TURN_DISPATCH, &payload)
                .await
            {
                Ok(SettleResult::Accepted { sequence }) => return Ok(sequence),
                Ok(SettleResult::Rejected { message }) => {
                    self.remove_pending(thread_id, message_id);
                    return Err(SendError::Rejected(message));
                }
                // Repeating settlement is safe: it only reads or rejects the
                // original command identity, never starts it.
                Err(error) if error.is_retryable() => {
                    tokio::time::sleep(backoff.next_delay()).await;
                }
                Err(error) => return Err(SendError::Unknown(error.to_string())),
            }
        }
    }

    /// Interrupts the running turn, if any.
    pub async fn interrupt(&self, thread_id: &ThreadId) -> Result<u64, SendError> {
        let turn_id = self.state().threads.get(thread_id).and_then(|store| {
            let thread = store.thread()?;
            thread
                .session
                .as_ref()
                .and_then(|s| s.active_turn_id.clone())
                .or_else(|| {
                    thread
                        .latest_turn
                        .as_ref()
                        .filter(|t| t.is_running())
                        .map(|t| t.turn_id.clone())
                })
        });
        self.dispatch(&ClientCommand::interrupt(thread_id.clone(), turn_id))
            .await
    }

    fn add_pending(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        text: String,
        created_at: String,
    ) {
        let change = {
            let mut state = self.shared.lock();
            state
                .threads
                .get_mut(thread_id)
                .map(|store| store.add_pending_user_message(message_id.clone(), text, created_at))
        };
        if let Some(change) = change.filter(|c| !c.is_empty()) {
            self.shared.emit(SessionEvent::Thread {
                thread_id: thread_id.clone(),
                change,
            });
        }
    }

    fn remove_pending(&self, thread_id: &ThreadId, message_id: &MessageId) {
        let change = {
            let mut state = self.shared.lock();
            state
                .threads
                .get_mut(thread_id)
                .map(|store| store.remove_pending_user_message(message_id))
        };
        if let Some(change) = change.filter(|c| !c.is_empty()) {
            self.shared.emit(SessionEvent::Thread {
                thread_id: thread_id.clone(),
                change,
            });
        }
    }
}

enum Ended {
    Shutdown,
    Reconnect,
    Disconnected(Disconnect),
}

async fn connect_once(
    http: &reqwest::Client,
    offer: &ClientOffer,
    config: &SessionConfig,
) -> Result<(NegotiateResult, RpcClient), ConnectError> {
    let negotiated = negotiate(http, &config.url, offer).await?;
    let url = offer.connect_url(&config.url, &negotiated, config.token.as_deref());
    let rpc = ClientConfig {
        bearer: config.bearer.clone().or_else(|| config.rpc.bearer.clone()),
        ..config.rpc.clone()
    };
    let client = tokio::time::timeout(Duration::from_secs(15), RpcClient::connect(&url, rpc))
        .await
        .map_err(|_| ConnectError::Ws("upgrade timed out".into()))??;
    Ok((negotiated, client))
}

async fn run(
    shared: Arc<Shared>,
    config: SessionConfig,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(http) => http,
        Err(error) => {
            shared.set_connection(ConnectionState::Waiting {
                reason: format!("http client: {error}"),
                retry_in: Duration::MAX,
            });
            return;
        }
    };
    let offer = ClientOffer::native(&config.client_version);
    let mut backoff = config.backoff.clone();
    let mut last_instance: Option<String> = None;

    loop {
        shared.set_connection(ConnectionState::Connecting {
            attempt: backoff.attempt(),
        });
        let reason = match connect_once(&http, &offer, &config).await {
            Ok((negotiated, client)) => {
                backoff.reset();
                if last_instance.as_deref() != Some(negotiated.server_instance_id.as_str()) {
                    if last_instance.is_some() {
                        tracing::info!(instance = %negotiated.server_instance_id, "server instance changed; dropping cursors");
                        shared.lock().forget_cursors();
                        shared.emit(SessionEvent::Shell(ShellChange {
                            reset: true,
                            ..ShellChange::default()
                        }));
                    }
                    last_instance = Some(negotiated.server_instance_id.clone());
                }
                shared.client.send_replace(Some(client.clone()));
                shared.set_connection(ConnectionState::Connected {
                    server_build: negotiated.server_build.clone(),
                    server_instance_id: negotiated.server_instance_id.clone(),
                });
                let ended = connected(&shared, &client, &mut control).await;
                shared.client.send_replace(None);
                client.close();
                match ended {
                    Ended::Shutdown => return,
                    Ended::Reconnect => continue,
                    Ended::Disconnected(reason) => reason.to_string(),
                }
            }
            Err(ConnectError::Compatibility(error)) if !error.renegotiate_fixes() => {
                shared.set_connection(ConnectionState::Incompatible(error));
                // Only an explicit reconnect retries; Open/Close are already
                // recorded in the state and apply on the next connect.
                loop {
                    match control.recv().await {
                        None => return,
                        Some(Control::Reconnect) => break,
                        Some(_) => {}
                    }
                }
                backoff.reset();
                continue;
            }
            Err(error) => error.to_string(),
        };

        let delay = backoff.next_delay();
        shared.set_connection(ConnectionState::Waiting {
            reason,
            retry_in: delay,
        });
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                command = control.recv() => match command {
                    None => return,
                    Some(Control::Reconnect) => {
                        backoff.reset();
                        break;
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

struct Tasks(HashMap<ThreadId, JoinHandle<()>>, Option<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
        if let Some(task) = &self.1 {
            task.abort();
        }
    }
}

async fn connected(
    shared: &Arc<Shared>,
    client: &RpcClient,
    control: &mut mpsc::UnboundedReceiver<Control>,
) -> Ended {
    let mut tasks = Tasks(
        HashMap::new(),
        Some(tokio::spawn(shell_loop(shared.clone(), client.clone()))),
    );
    let open: Vec<ThreadId> = shared.lock().open.clone();
    for thread_id in open {
        let task = tokio::spawn(thread_loop(
            shared.clone(),
            client.clone(),
            thread_id.clone(),
        ));
        tasks.0.insert(thread_id, task);
    }
    loop {
        tokio::select! {
            reason = client.closed() => return Ended::Disconnected(reason),
            command = control.recv() => match command {
                None => return Ended::Shutdown,
                Some(Control::Reconnect) => return Ended::Reconnect,
                Some(Control::Open(thread_id)) => {
                    let running = tasks.0.get(&thread_id).is_some_and(|t| !t.is_finished());
                    if !running && shared.lock().open.contains(&thread_id) {
                        let task = tokio::spawn(thread_loop(shared.clone(), client.clone(), thread_id.clone()));
                        tasks.0.insert(thread_id, task);
                    }
                }
                Some(Control::Close(thread_id)) => {
                    if let Some(task) = tasks.0.remove(&thread_id) {
                        task.abort();
                    }
                }
            },
        }
    }
}

async fn shell_loop(shared: Arc<Shared>, client: RpcClient) {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(10));
    loop {
        match client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_SHELL, json!({}))
            .await
        {
            Ok(mut stream) => {
                while let Some(item) = stream.next_as::<ShellStreamItem>().await {
                    match item {
                        Ok(item) => {
                            backoff.reset();
                            let change = shared.lock().shell.apply(item);
                            if !change.is_empty() {
                                shared.emit(SessionEvent::Shell(change));
                            }
                        }
                        Err(RpcError::Disconnected(_)) => return,
                        Err(error) => {
                            // A skipped item could hide a removal; resubscribe for a snapshot.
                            tracing::warn!(%error, "shell stream failed; resubscribing");
                            break;
                        }
                    }
                }
            }
            Err(RpcError::Disconnected(_)) => return,
            Err(error) => tracing::warn!(%error, "shell subscribe failed"),
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

async fn thread_loop(shared: Arc<Shared>, client: RpcClient, thread_id: ThreadId) {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(10));
    loop {
        let cursor = shared
            .lock()
            .threads
            .get(&thread_id)
            .and_then(ThreadStore::resume_cursor);
        let input = SubscribeThreadInput::new(thread_id.clone(), cursor);
        let payload = serde_json::to_value(input).unwrap_or_default();
        match client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_THREAD, payload)
            .await
        {
            Ok(mut stream) => {
                while let Some(item) = stream.next_as::<ThreadStreamItem>().await {
                    match item {
                        Ok(item) => {
                            backoff.reset();
                            let (change, status) = {
                                let mut state = shared.lock();
                                let store = state
                                    .threads
                                    .entry(thread_id.clone())
                                    .or_insert_with(|| ThreadStore::new(thread_id.clone()));
                                (store.apply(item), store.status())
                            };
                            if !change.is_empty() {
                                shared.emit(SessionEvent::Thread {
                                    thread_id: thread_id.clone(),
                                    change,
                                });
                            }
                            match status {
                                // History changed underneath; resubscribe without a cursor.
                                ThreadStatus::NeedsSnapshot => break,
                                ThreadStatus::Deleted => return,
                                ThreadStatus::Loading | ThreadStatus::Ready => {}
                            }
                        }
                        Err(RpcError::Disconnected(_)) => return,
                        Err(error)
                            if !error.is_retryable() && matches!(error, RpcError::Failure(_)) =>
                        {
                            shared.emit(SessionEvent::ThreadFailed {
                                thread_id: thread_id.clone(),
                                error,
                            });
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(%thread_id, %error, "thread stream failed; resubscribing");
                            if let Some(store) = shared.lock().threads.get_mut(&thread_id) {
                                // A dropped item means the cursor cannot be trusted.
                                store.reset_cursor();
                            }
                            break;
                        }
                    }
                }
            }
            Err(RpcError::Disconnected(_)) => return,
            Err(error @ RpcError::Failure(_)) => {
                shared.emit(SessionEvent::ThreadFailed {
                    thread_id: thread_id.clone(),
                    error,
                });
                return;
            }
            Err(error) => tracing::warn!(%thread_id, %error, "thread subscribe failed"),
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_past_the_limit_evicts_the_oldest() {
        let mut state = SessionState::new();
        let ids: Vec<ThreadId> = (0..=STREAM_LIMIT_THREAD)
            .map(|i| ThreadId::from(format!("t-{i}")))
            .collect();
        for id in &ids[..STREAM_LIMIT_THREAD] {
            assert_eq!(state.open(id), None);
        }
        // Re-opening refreshes recency instead of duplicating.
        assert_eq!(state.open(&ids[0]), None);
        assert_eq!(state.open_threads().len(), STREAM_LIMIT_THREAD);
        assert_eq!(state.open(&ids[STREAM_LIMIT_THREAD]), Some(ids[1].clone()));
        assert!(state.open_threads().contains(&ids[0]));
        assert!(state.close(&ids[0]));
        assert!(!state.close(&ids[0]));
        // Stores outlive their subscription so a reopen can resume.
        assert!(state.threads.contains_key(&ids[1]));
    }

    #[tokio::test]
    async fn unreachable_server_waits_with_backoff() {
        let mut config = SessionConfig::new(Url::parse("http://127.0.0.1:9").unwrap());
        config.backoff = Backoff::new(Duration::from_millis(10), Duration::from_millis(20));
        let (session, mut events) = Session::start(config);
        let mut saw_waiting = 0;
        while saw_waiting < 2 {
            match tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .unwrap()
            {
                Some(SessionEvent::Connection(ConnectionState::Waiting { .. })) => saw_waiting += 1,
                Some(_) => {}
                None => panic!("session ended"),
            }
        }
        assert!(!session.is_connected());
        let result = session
            .send_message(
                &"t".into(),
                "hi".into(),
                RuntimeMode::FullAccess,
                InteractionMode::Default,
            )
            .await;
        assert_eq!(result, Err(SendError::NotConnected));
    }
}
