use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use synara_protocol::frames::{decode_server_frames, ClientFrame, RequestId, ServerFrame};
use synara_protocol::generated::{STREAM_LIMIT_THREAD, STREAM_LIMIT_TOTAL};
use synara_protocol::negotiate::CompatibilityError;
use synara_protocol::tags;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use url::Url;

use crate::error::{ConnectError, Disconnect, RpcError};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub ping_interval: Duration,
    /// A ping unanswered for this long ends the connection.
    pub pong_timeout: Duration,
    /// `Authorization: Bearer …` for the upgrade (remote hosts). Loopback
    /// servers take `?token=` in the URL instead.
    pub bearer: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(10),
            pong_timeout: Duration::from_secs(25),
            bearer: None,
        }
    }
}

enum Command {
    Call {
        tag: String,
        payload: Value,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
    Stream {
        tag: String,
        payload: Value,
        events: mpsc::UnboundedSender<StreamEvent>,
        reply: oneshot::Sender<Result<RequestId, RpcError>>,
    },
    Ack(RequestId),
    Interrupt(RequestId),
    Close,
}

enum StreamEvent {
    Chunk(Vec<Value>),
    End(Result<(), RpcError>),
}

enum Pending {
    Call(oneshot::Sender<Result<Value, RpcError>>),
    Stream {
        tag: String,
        events: mpsc::UnboundedSender<StreamEvent>,
    },
}

/// Handle to one live connection. Clone freely; the connection closes when
/// [`RpcClient::close`] is called or every handle is dropped.
#[derive(Clone)]
pub struct RpcClient {
    commands: mpsc::UnboundedSender<Command>,
    closed: watch::Receiver<Option<Disconnect>>,
    _owner: Arc<CloseOnDrop>,
}

struct CloseOnDrop(mpsc::UnboundedSender<Command>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Command::Close);
    }
}

impl RpcClient {
    /// Opens the socket. `url` is the `ws(s)://…/ws` URL from
    /// [`synara_protocol::negotiate::ClientOffer::connect_url`].
    pub async fn connect(url: &Url, config: ClientConfig) -> Result<Self, ConnectError> {
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError::Ws(e.to_string()))?;
        if let Some(bearer) = &config.bearer {
            let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
                .map_err(|e| ConnectError::Ws(e.to_string()))?;
            request.headers_mut().insert("Authorization", value);
        }
        let (socket, _) = match tokio_tungstenite::connect_async(request).await {
            Ok(ok) => ok,
            Err(tungstenite::Error::Http(response)) => {
                let status = response.status().as_u16();
                let body = response
                    .body()
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                if status == 426 {
                    if let Ok(error) = serde_json::from_str::<CompatibilityError>(&body) {
                        return Err(ConnectError::Compatibility(error));
                    }
                }
                return Err(ConnectError::Http { status, body });
            }
            Err(error) => return Err(ConnectError::Ws(error.to_string())),
        };
        Ok(Self::spawn(socket, config))
    }

    fn spawn<S>(socket: S, config: ClientConfig) -> Self
    where
        S: futures_util::Stream<Item = Result<Message, tungstenite::Error>>
            + futures_util::Sink<Message, Error = tungstenite::Error>
            + Unpin
            + Send
            + 'static,
    {
        let (commands, inbox) = mpsc::unbounded_channel();
        let (closed_tx, closed) = watch::channel(None);
        tokio::spawn(async move {
            let reason = Actor::new(config).run(socket, inbox).await;
            tracing::debug!(%reason, "rpc connection ended");
            let _ = closed_tx.send(Some(reason));
        });
        Self {
            _owner: Arc::new(CloseOnDrop(commands.clone())),
            commands,
            closed,
        }
    }

    fn disconnected(&self) -> RpcError {
        RpcError::Disconnected(
            self.closed
                .borrow()
                .clone()
                .unwrap_or(Disconnect::ClientClosed),
        )
    }

    /// A unary RPC returning the raw success value.
    pub async fn call_value(&self, tag: &str, payload: Value) -> Result<Value, RpcError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Call {
                tag: tag.to_owned(),
                payload,
                reply,
            })
            .map_err(|_| self.disconnected())?;
        response.await.map_err(|_| self.disconnected())?
    }

    /// A unary RPC with typed input and output.
    pub async fn call<P: Serialize, T: DeserializeOwned>(
        &self,
        tag: &str,
        payload: &P,
    ) -> Result<T, RpcError> {
        let payload = serde_json::to_value(payload).map_err(|e| RpcError::Decode(e.to_string()))?;
        let value = self.call_value(tag, payload).await?;
        serde_json::from_value(value).map_err(|e| RpcError::Decode(e.to_string()))
    }

    /// Opens a streaming RPC.
    pub async fn stream(&self, tag: &str, payload: Value) -> Result<RpcStream, RpcError> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Stream {
                tag: tag.to_owned(),
                payload,
                events: events_tx,
                reply,
            })
            .map_err(|_| self.disconnected())?;
        let id = response.await.map_err(|_| self.disconnected())??;
        Ok(RpcStream {
            id,
            events,
            buffer: VecDeque::new(),
            commands: self.commands.clone(),
            done: false,
        })
    }

    /// Resolves when the connection ends, with the reason.
    pub async fn closed(&self) -> Disconnect {
        let mut closed = self.closed.clone();
        loop {
            if let Some(reason) = closed.borrow_and_update().clone() {
                return reason;
            }
            if closed.changed().await.is_err() {
                return Disconnect::ClientClosed;
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }
}

/// Items of one streaming RPC. Pulling a chunk acks it to the server.
pub struct RpcStream {
    id: RequestId,
    events: mpsc::UnboundedReceiver<StreamEvent>,
    buffer: VecDeque<Value>,
    commands: mpsc::UnboundedSender<Command>,
    done: bool,
}

impl RpcStream {
    pub fn request_id(&self) -> RequestId {
        self.id
    }

    /// The next item; `None` once the stream completed. An error is terminal.
    pub async fn next(&mut self) -> Option<Result<Value, RpcError>> {
        loop {
            if let Some(value) = self.buffer.pop_front() {
                return Some(Ok(value));
            }
            if self.done {
                return None;
            }
            match self.events.recv().await {
                Some(StreamEvent::Chunk(values)) => {
                    let _ = self.commands.send(Command::Ack(self.id));
                    self.buffer.extend(values);
                }
                Some(StreamEvent::End(Ok(()))) => {
                    self.done = true;
                }
                Some(StreamEvent::End(Err(error))) => {
                    self.done = true;
                    return Some(Err(error));
                }
                None => {
                    self.done = true;
                    return Some(Err(RpcError::Disconnected(Disconnect::ClientClosed)));
                }
            }
        }
    }

    /// The next item decoded as `T`.
    pub async fn next_as<T: DeserializeOwned>(&mut self) -> Option<Result<T, RpcError>> {
        Some(self.next().await?.and_then(|value| {
            serde_json::from_value(value).map_err(|e| RpcError::Decode(e.to_string()))
        }))
    }
}

impl Drop for RpcStream {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.commands.send(Command::Interrupt(self.id));
        }
    }
}

struct Actor {
    config: ClientConfig,
    next_id: u64,
    pending: HashMap<RequestId, Pending>,
    ping_sent_at: Option<Instant>,
}

impl Actor {
    fn new(config: ClientConfig) -> Self {
        Self {
            config,
            next_id: 1,
            pending: HashMap::new(),
            ping_sent_at: None,
        }
    }

    fn allocate_id(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        id
    }

    fn open_streams(&self, tag: Option<&str>) -> usize {
        self.pending
            .values()
            .filter(|p| match p {
                Pending::Stream { tag: open, .. } => tag.is_none_or(|t| t == open),
                Pending::Call(_) => false,
            })
            .count()
    }

    fn check_stream_limit(&self, tag: &str) -> Result<(), RpcError> {
        if self.open_streams(None) >= STREAM_LIMIT_TOTAL {
            return Err(RpcError::StreamLimit {
                tag: tag.to_owned(),
                limit: STREAM_LIMIT_TOTAL,
            });
        }
        if tag == tags::ORCHESTRATION_SUBSCRIBE_THREAD
            && self.open_streams(Some(tag)) >= STREAM_LIMIT_THREAD
        {
            return Err(RpcError::StreamLimit {
                tag: tag.to_owned(),
                limit: STREAM_LIMIT_THREAD,
            });
        }
        Ok(())
    }

    async fn run<S>(mut self, socket: S, mut inbox: mpsc::UnboundedReceiver<Command>) -> Disconnect
    where
        S: futures_util::Stream<Item = Result<Message, tungstenite::Error>>
            + futures_util::Sink<Message, Error = tungstenite::Error>
            + Unpin,
    {
        let (mut sink, mut source) = socket.split();
        let mut ping = tokio::time::interval(self.config.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;

        let reason = loop {
            let outgoing: Vec<ClientFrame> = tokio::select! {
                command = inbox.recv() => match command {
                    None | Some(Command::Close) => {
                        let _ = sink.send(Message::Close(None)).await;
                        break Disconnect::ClientClosed;
                    }
                    Some(command) => self.handle_command(command),
                },
                message = source.next() => match message {
                    Some(Ok(Message::Text(text))) => match decode_server_frames(text.as_str()) {
                        Ok(frames) => {
                            let mut out = Vec::new();
                            let mut fatal = None;
                            for frame in frames {
                                match self.handle_frame(frame) {
                                    Ok(Some(frame)) => out.push(frame),
                                    Ok(None) => {}
                                    Err(reason) => {
                                        fatal = Some(reason);
                                        break;
                                    }
                                }
                            }
                            if let Some(reason) = fatal {
                                break reason;
                            }
                            out
                        }
                        Err(error) => break Disconnect::Malformed(error.to_string()),
                    },
                    Some(Ok(Message::Close(_))) | None => break Disconnect::Closed,
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(error)) => break Disconnect::Transport(error.to_string()),
                },
                _ = ping.tick() => {
                    match self.ping_sent_at {
                        Some(sent) if sent.elapsed() >= self.config.pong_timeout => {
                            break Disconnect::PongTimeout;
                        }
                        Some(_) => Vec::new(),
                        None => {
                            self.ping_sent_at = Some(Instant::now());
                            vec![ClientFrame::Ping]
                        }
                    }
                }
            };
            let mut failed = None;
            for frame in outgoing {
                if let Err(error) = sink.feed(Message::text(frame.to_json())).await {
                    failed = Some(Disconnect::Transport(error.to_string()));
                    break;
                }
            }
            if failed.is_none() {
                if let Err(error) = sink.flush().await {
                    failed = Some(Disconnect::Transport(error.to_string()));
                }
            }
            if let Some(reason) = failed {
                break reason;
            }
        };

        for (_, pending) in self.pending.drain() {
            let error = RpcError::Disconnected(reason.clone());
            match pending {
                Pending::Call(reply) => {
                    let _ = reply.send(Err(error));
                }
                Pending::Stream { events, .. } => {
                    let _ = events.send(StreamEvent::End(Err(error)));
                }
            }
        }
        // Commands queued after the end get a closed oneshot, which callers map
        // to the recorded reason.
        inbox.close();
        reason
    }

    fn handle_command(&mut self, command: Command) -> Vec<ClientFrame> {
        match command {
            Command::Call {
                tag,
                payload,
                reply,
            } => {
                let id = self.allocate_id();
                self.pending.insert(id, Pending::Call(reply));
                vec![ClientFrame::request(id, tag, payload)]
            }
            Command::Stream {
                tag,
                payload,
                events,
                reply,
            } => {
                if let Err(error) = self.check_stream_limit(&tag) {
                    let _ = reply.send(Err(error));
                    return Vec::new();
                }
                let id = self.allocate_id();
                if reply.send(Ok(id)).is_err() {
                    // The caller gave up before the stream opened.
                    return Vec::new();
                }
                self.pending.insert(
                    id,
                    Pending::Stream {
                        tag: tag.clone(),
                        events,
                    },
                );
                vec![ClientFrame::request(id, tag, payload)]
            }
            Command::Ack(request_id) => {
                if self.pending.contains_key(&request_id) {
                    vec![ClientFrame::Ack { request_id }]
                } else {
                    Vec::new()
                }
            }
            Command::Interrupt(request_id) => {
                if self.pending.remove(&request_id).is_some() {
                    vec![ClientFrame::Interrupt {
                        request_id,
                        interruptors: Vec::new(),
                    }]
                } else {
                    Vec::new()
                }
            }
            Command::Close => unreachable!("handled in run"),
        }
    }

    /// Returns a frame to send back, or the reason the connection must end.
    fn handle_frame(&mut self, frame: ServerFrame) -> Result<Option<ClientFrame>, Disconnect> {
        match frame {
            ServerFrame::Chunk { request_id, values } => match self.pending.get(&request_id) {
                Some(Pending::Stream { events, .. }) => {
                    if events.send(StreamEvent::Chunk(values)).is_err() {
                        // Consumer is gone; its Drop interrupt may still be queued.
                        self.pending.remove(&request_id);
                        return Ok(Some(ClientFrame::Interrupt {
                            request_id,
                            interruptors: Vec::new(),
                        }));
                    }
                    Ok(None)
                }
                Some(Pending::Call(_)) => {
                    tracing::warn!(%request_id, "chunk for a unary rpc; ignored");
                    Ok(None)
                }
                // Late chunk for an interrupted stream.
                None => Ok(None),
            },
            ServerFrame::Exit { request_id, exit } => {
                let result = exit.into_result().map_err(RpcError::Failure);
                match self.pending.remove(&request_id) {
                    Some(Pending::Call(reply)) => {
                        let _ = reply.send(result);
                    }
                    Some(Pending::Stream { events, .. }) => {
                        let _ = events.send(StreamEvent::End(result.map(|_| ())));
                    }
                    None => {}
                }
                Ok(None)
            }
            ServerFrame::Pong => {
                self.ping_sent_at = None;
                Ok(None)
            }
            ServerFrame::Defect { defect } => Err(Disconnect::Defect(defect)),
            ServerFrame::ClientProtocolError { error } => Err(Disconnect::Protocol(error)),
            ServerFrame::Request { tag } => {
                tracing::debug!(%tag, "ignoring server-initiated request");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::WebSocketStream;

    type ServerSocket = WebSocketStream<tokio::net::TcpStream>;

    /// Starts a one-connection server running `script`, returns its ws URL.
    async fn serve<F, Fut>(script: F) -> Url
    where
        F: FnOnce(ServerSocket) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            script(socket).await;
        });
        Url::parse(&format!("ws://{addr}/ws")).unwrap()
    }

    async fn recv(socket: &mut ServerSocket) -> Value {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Close(_) => panic!("client closed"),
                _ => {}
            }
        }
    }

    /// Next frame that is not a Ping.
    async fn recv_frame(socket: &mut ServerSocket) -> Value {
        loop {
            let frame = recv(socket).await;
            if frame["_tag"] != "Ping" {
                return frame;
            }
        }
    }

    async fn send(socket: &mut ServerSocket, value: Value) {
        socket.send(Message::text(value.to_string())).await.unwrap();
    }

    fn quiet() -> ClientConfig {
        ClientConfig {
            ping_interval: Duration::from_secs(3600),
            pong_timeout: Duration::from_secs(3600),
            bearer: None,
        }
    }

    #[tokio::test]
    async fn unary_success_and_typed_failure() {
        let url = serve(|mut socket| async move {
            let first = recv_frame(&mut socket).await;
            assert_eq!(first["_tag"], "Request");
            assert_eq!(first["tag"], "server.getConfig");
            send(&mut socket, json!({"_tag":"Exit","requestId":first["id"],"exit":{"_tag":"Success","value":{"ok":1}}})).await;
            let second = recv_frame(&mut socket).await;
            send(&mut socket, json!({"_tag":"Exit","requestId":second["id"],"exit":{"_tag":"Failure","cause":[
                {"_tag":"Fail","error":{"_tag":"WsRpcError","message":"nope","code":"X","retryable":true}}]}})).await;
            let _ = socket.next().await;
        })
        .await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let ok = client
            .call_value("server.getConfig", json!({}))
            .await
            .unwrap();
        assert_eq!(ok, json!({"ok":1}));
        let err = client
            .call_value("server.getConfig", json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("X"));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn stream_waits_for_ack_between_chunks() {
        let url = serve(|mut socket| async move {
            let request = recv_frame(&mut socket).await;
            let id = request["id"].clone();
            // Batched: two frames in one message.
            send(
                &mut socket,
                json!([{"_tag":"Chunk","requestId":id,"values":[1,2]}]),
            )
            .await;
            let ack = recv_frame(&mut socket).await;
            assert_eq!(ack, json!({"_tag":"Ack","requestId":id}));
            send(
                &mut socket,
                json!({"_tag":"Chunk","requestId":id,"values":[3]}),
            )
            .await;
            let ack = recv_frame(&mut socket).await;
            assert_eq!(ack["_tag"], "Ack");
            send(
                &mut socket,
                json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Success","value":null}}),
            )
            .await;
            let _ = socket.next().await;
        })
        .await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let mut stream = client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_SHELL, json!({}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(item) = stream.next().await {
            seen.push(item.unwrap());
        }
        assert_eq!(seen, vec![json!(1), json!(2), json!(3)]);
    }

    #[tokio::test]
    async fn dropping_a_stream_interrupts_it() {
        let (seen_tx, seen_rx) = oneshot::channel();
        let url = serve(|mut socket| async move {
            let request = recv_frame(&mut socket).await;
            let id = request["id"].clone();
            send(
                &mut socket,
                json!({"_tag":"Chunk","requestId":id,"values":["a"]}),
            )
            .await;
            let mut frames = Vec::new();
            loop {
                let frame = recv_frame(&mut socket).await;
                let done = frame["_tag"] == "Interrupt";
                frames.push(frame);
                if done {
                    break;
                }
            }
            let _ = seen_tx.send(frames);
            let _ = socket.next().await;
        })
        .await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let mut stream = client
            .stream(
                tags::ORCHESTRATION_SUBSCRIBE_THREAD,
                json!({"threadId":"t"}),
            )
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), json!("a"));
        let id = stream.request_id().0;
        drop(stream);
        let frames = seen_rx.await.unwrap();
        assert_eq!(
            frames.last().unwrap(),
            &json!({"_tag":"Interrupt","requestId":id,"interruptors":[]})
        );
    }

    #[tokio::test]
    async fn defect_fails_every_pending_request() {
        let url = serve(|mut socket| async move {
            recv_frame(&mut socket).await;
            recv_frame(&mut socket).await;
            send(&mut socket, json!({"_tag":"Defect","defect":"boom"})).await;
            let _ = socket.next().await;
        })
        .await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let mut stream = client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_SHELL, json!({}))
            .await
            .unwrap();
        let call = client.call_value("server.getConfig", json!({}));
        let (call, item) = tokio::join!(call, stream.next());
        assert_eq!(
            call.unwrap_err(),
            RpcError::Disconnected(Disconnect::Defect(json!("boom")))
        );
        assert!(matches!(
            item,
            Some(Err(RpcError::Disconnected(Disconnect::Defect(_))))
        ));
        assert_eq!(client.closed().await, Disconnect::Defect(json!("boom")));
        assert!(client.call_value("x", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn enforces_the_thread_stream_limit() {
        let url = serve(|mut socket| async move { while socket.next().await.is_some() {} }).await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let mut open = Vec::new();
        for _ in 0..STREAM_LIMIT_THREAD {
            open.push(
                client
                    .stream(tags::ORCHESTRATION_SUBSCRIBE_THREAD, json!({}))
                    .await
                    .unwrap(),
            );
        }
        let over = client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_THREAD, json!({}))
            .await;
        assert!(
            matches!(over, Err(RpcError::StreamLimit { limit, .. }) if limit == STREAM_LIMIT_THREAD)
        );
        // Other streams still fit under the total limit.
        open.push(
            client
                .stream(tags::ORCHESTRATION_SUBSCRIBE_SHELL, json!({}))
                .await
                .unwrap(),
        );
        // Dropping one frees a thread slot.
        open.remove(0);
        assert!(client
            .stream(tags::ORCHESTRATION_SUBSCRIBE_THREAD, json!({}))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn pings_and_times_out_without_pong() {
        let url = serve(|mut socket| async move {
            let first = recv(&mut socket).await;
            assert_eq!(first["_tag"], "Ping");
            send(&mut socket, json!({"_tag":"Pong"})).await;
            // Second ping goes unanswered.
            while socket.next().await.is_some() {}
        })
        .await;
        let config = ClientConfig {
            ping_interval: Duration::from_millis(40),
            pong_timeout: Duration::from_millis(100),
            bearer: None,
        };
        let client = RpcClient::connect(&url, config).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(5), client.closed())
            .await
            .unwrap();
        assert_eq!(reason, Disconnect::PongTimeout);
    }

    #[tokio::test]
    async fn server_close_is_reported() {
        let url = serve(|mut socket| async move {
            recv_frame(&mut socket).await;
            socket.close(None).await.unwrap();
        })
        .await;
        let client = RpcClient::connect(&url, quiet()).await.unwrap();
        let err = client
            .call_value("server.getConfig", json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Disconnected(Disconnect::Closed));
    }

    #[tokio::test]
    async fn upgrade_426_decodes_compatibility_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let body = r#"{"_tag":"WsCompatibilityError","message":"restarted","code":"WS_SERVER_GENERATION_CHANGED","action":"reload","serverBuild":"b","protocolEpoch":1,"minRevision":3,"maxRevision":3}"#;
            let response = format!(
                "HTTP/1.1 426 Upgrade Required\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let url = Url::parse(&format!("ws://{addr}/ws")).unwrap();
        let Err(ConnectError::Compatibility(error)) = RpcClient::connect(&url, quiet()).await
        else {
            panic!("expected a compatibility error");
        };
        assert!(error.renegotiate_fixes());
    }
}
