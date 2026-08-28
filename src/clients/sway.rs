//! Minimal asynchronous client for the Sway/i3 IPC wire protocol.
//!
//! Ironbar only needs commands, workspace/input snapshots, and three event
//! streams. Keeping that surface local avoids pulling the compositor's full
//! recursive tree schema into the bar and lets Scroll add harmless fields (or
//! orientation spellings) without breaking event decoding.

use crate::spawn;
use serde::Deserialize;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, trace, warn};

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LEN: usize = 14;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const EVENT_BIT: u32 = 1 << 31;
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_INPUTS: u32 = 100;
const WORKSPACE_EVENT: u32 = EVENT_BIT;
const MODE_EVENT: u32 = EVENT_BIT | 2;
const INPUT_EVENT: u32 = EVENT_BIT | 21;
const RECONNECT_INITIAL: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SUBSCRIPTION_ACK_TIMEOUT: Duration = Duration::from_secs(2);

type Result<T> = std::result::Result<T, Error>;
type SyncFn<T> = dyn Fn(&T) + Sync + Send;
type ConnectFuture = Pin<Box<dyn Future<Output = Result<Connection>> + Send>>;
type Connector = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

#[derive(Debug, Error)]
pub enum Error {
    #[error("SWAYSOCK is not set")]
    MissingSocket,
    #[error("failed to connect to Sway IPC socket {path:?}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Sway IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("Sway IPC frame has invalid magic")]
    InvalidMagic,
    #[error("Sway IPC frame is {length} bytes; the limit is {max}")]
    FrameTooLarge { length: usize, max: usize },
    #[error("Sway IPC replied with message type {actual}, expected {expected}")]
    UnexpectedMessageType { expected: u32, actual: u32 },
    #[error("invalid {context} JSON from Sway: {source}")]
    InvalidJson {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("Sway rejected command: {reason}")]
    CommandRejected { reason: String },
    #[error("Sway rejected event subscription: {reason}")]
    SubscriptionRejected { reason: String },
    #[error("Sway IPC {operation} timed out after {millis} ms")]
    RequestTimeout {
        operation: &'static str,
        millis: u128,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: i64,
    pub num: i64,
    pub name: String,
    pub output: String,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub focused: bool,
}

/// The deliberately small subset of a compositor node used by workspace UI.
/// All recursive tree fields, including `layout`, are ignored by serde.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub id: i64,
    #[serde(default)]
    pub num: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub urgent: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Input {
    pub identifier: String,
    #[serde(default)]
    pub xkb_active_layout_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChange {
    Init,
    Empty,
    Focus,
    Move,
    Rename,
    Urgent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEvent {
    pub change: WorkspaceChange,
    #[serde(default)]
    pub current: Option<Node>,
    #[serde(default)]
    pub old: Option<Node>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputChange {
    XkbLayout,
    XkbKeymap,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct InputEvent {
    pub change: InputChange,
    pub input: Input,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModeEvent {
    pub change: String,
    #[serde(default)]
    pub pango_markup: bool,
}

#[derive(Debug)]
struct Frame {
    message_type: u32,
    payload: Vec<u8>,
}

#[derive(Debug)]
enum Event {
    Workspace(WorkspaceEvent),
    Input(InputEvent),
    Mode(ModeEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventType {
    Workspace,
    Input,
    Mode,
}

impl EventType {
    const fn wire_name(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Input => "input",
            Self::Mode => "mode",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Workspace => 0,
            Self::Input => 1,
            Self::Mode => 2,
        }
    }
}

pub(crate) struct Connection {
    io: Box<dyn AsyncIo>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

impl Connection {
    async fn new() -> Result<Self> {
        let path = sway_socket_path()?;
        let stream = UnixStream::connect(&path)
            .await
            .map_err(|source| Error::Connect {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            io: Box::new(stream),
        })
    }

    #[cfg(test)]
    fn from_io(io: impl AsyncIo + 'static) -> Self {
        Self { io: Box::new(io) }
    }

    pub(crate) async fn run_command(&mut self, command: String) -> Result<()> {
        let payload = transact(&mut self.io, RUN_COMMAND, command.as_bytes()).await?;
        let outcomes: Vec<CommandOutcome> = decode_json(&payload, "command response")?;
        let failures = outcomes
            .into_iter()
            .filter(|outcome| !outcome.success)
            .map(|outcome| {
                outcome.error.unwrap_or_else(|| {
                    if outcome.parse_error {
                        "command parse error".to_string()
                    } else {
                        "command rejected".to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::CommandRejected {
                reason: failures.join("; "),
            })
        }
    }

    pub(crate) async fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        let payload = transact(&mut self.io, GET_WORKSPACES, &[]).await?;
        decode_json(&payload, "workspace response")
    }

    pub(crate) async fn get_inputs(&mut self) -> Result<Vec<Input>> {
        let payload = transact(&mut self.io, GET_INPUTS, &[]).await?;
        decode_json(&payload, "input response")
    }

    async fn subscribe(&mut self, event_types: &[EventType]) -> Result<()> {
        let names = event_types
            .iter()
            .map(|event_type| event_type.wire_name())
            .collect::<Vec<_>>();
        let request = serde_json::to_vec(&names).map_err(|source| Error::InvalidJson {
            context: "subscription request",
            source,
        })?;
        let payload = transact(&mut self.io, SUBSCRIBE, &request).await?;
        let response: SubscriptionOutcome = decode_json(&payload, "subscription response")?;
        if response.success {
            Ok(())
        } else {
            Err(Error::SubscriptionRejected {
                reason: response
                    .error
                    .unwrap_or_else(|| "subscription rejected".to_string()),
            })
        }
    }

    async fn next_event(&mut self) -> Result<Option<Event>> {
        let frame = read_frame(&mut self.io).await?;
        decode_event(frame)
    }
}

pub(crate) struct RequestConnection {
    current: Option<Connection>,
    connect: Connector,
}

impl std::fmt::Debug for RequestConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestConnection")
            .field("connected", &self.current.is_some())
            .finish()
    }
}

impl RequestConnection {
    fn new(current: Connection, connect: Connector) -> Self {
        Self {
            current: Some(current),
            connect,
        }
    }

    async fn ensure_connected(&mut self) -> Result<&mut Connection> {
        if self.current.is_none() {
            self.current = Some((self.connect)().await?);
        }
        self.current.as_mut().ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "Sway IPC reconnect returned no connection",
            ))
        })
    }

    pub(crate) async fn run_command(&mut self, command: String) -> Result<()> {
        self.run_command_with_timeout(command, REQUEST_TIMEOUT)
            .await
    }

    async fn run_command_with_timeout(
        &mut self,
        command: String,
        request_timeout: Duration,
    ) -> Result<()> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.run_command(command).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "command",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    pub(crate) async fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        self.get_workspaces_with_timeout(REQUEST_TIMEOUT).await
    }

    async fn get_workspaces_with_timeout(
        &mut self,
        request_timeout: Duration,
    ) -> Result<Vec<Workspace>> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.get_workspaces().await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "workspace request",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    pub(crate) async fn get_inputs(&mut self) -> Result<Vec<Input>> {
        self.get_inputs_with_timeout(REQUEST_TIMEOUT).await
    }

    async fn get_inputs_with_timeout(&mut self, request_timeout: Duration) -> Result<Vec<Input>> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.get_inputs().await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "input request",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    fn finish_request<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.as_ref().is_err_and(Error::breaks_request_connection) {
            // Never retry the operation that just failed: a command may have
            // been accepted before its reply was lost. Retire only the broken
            // transport so the *next* independent operation reconnects.
            self.current = None;
        }
        result
    }
}

impl Error {
    fn breaks_request_connection(&self) -> bool {
        !matches!(self, Self::CommandRejected { .. })
    }
}

fn socket_connector() -> Connector {
    Arc::new(|| Box::pin(Connection::new()))
}

async fn connect_with_timeout(
    connect: &Connector,
    connection_timeout: Duration,
    operation: &'static str,
) -> Result<Connection> {
    tokio::time::timeout(connection_timeout, connect())
        .await
        .map_err(|_| Error::RequestTimeout {
            operation,
            millis: connection_timeout.as_millis(),
        })?
}

async fn connect_and_subscribe_with_timeout(
    connect: &Connector,
    event_types: &[EventType],
    acknowledgement_timeout: Duration,
) -> Result<Connection> {
    tokio::time::timeout(acknowledgement_timeout, async {
        let mut connection = connect().await?;
        connection.subscribe(event_types).await?;
        Ok(connection)
    })
    .await
    .map_err(|_| Error::RequestTimeout {
        operation: "subscription connection and acknowledgement",
        millis: acknowledgement_timeout.as_millis(),
    })?
}

#[derive(Debug, Deserialize)]
struct CommandOutcome {
    success: bool,
    #[serde(default)]
    parse_error: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionOutcome {
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Default)]
struct Listeners {
    workspaces: Vec<Arc<SyncFn<WorkspaceEvent>>>,
    inputs: Vec<Arc<SyncFn<InputEvent>>>,
    modes: Vec<Arc<SyncFn<ModeEvent>>>,
}

impl Listeners {
    fn push(&mut self, listener: Listener) {
        match listener {
            Listener::Workspace(listener) => self.workspaces.push(listener),
            Listener::Input(listener) => self.inputs.push(listener),
            Listener::Mode(listener) => self.modes.push(listener),
        }
    }

    fn dispatch(&self, event: &Event) {
        match event {
            Event::Workspace(event) => {
                for listener in &self.workspaces {
                    listener(event);
                }
            }
            Event::Input(event) => {
                for listener in &self.inputs {
                    listener(event);
                }
            }
            Event::Mode(event) => {
                for listener in &self.modes {
                    listener(event);
                }
            }
        }
    }
}

enum Listener {
    Workspace(Arc<SyncFn<WorkspaceEvent>>),
    Input(Arc<SyncFn<InputEvent>>),
    Mode(Arc<SyncFn<ModeEvent>>),
}

impl Listener {
    const fn event_type(&self) -> EventType {
        match self {
            Self::Workspace(_) => EventType::Workspace,
            Self::Input(_) => EventType::Input,
            Self::Mode(_) => EventType::Mode,
        }
    }
}

struct TaskState {
    join_handles: [Option<tokio::task::JoinHandle<()>>; 3],
    listeners: Arc<RwLock<Listeners>>,
}

pub struct Client {
    connection: Arc<Mutex<RequestConnection>>,
    task_state: Mutex<TaskState>,
    connect: Connector,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("connection", &"Connection")
            .field("task_state", &format_args!("<...>"))
            .finish()
    }
}

impl Client {
    pub(crate) async fn new() -> Result<Self> {
        let connect = socket_connector();
        let current = connect_with_timeout(&connect, REQUEST_TIMEOUT, "initial connection").await?;
        let connection = Arc::new(Mutex::new(RequestConnection::new(current, connect.clone())));
        info!("Sway IPC request client connected");

        Ok(Self {
            connection,
            task_state: Mutex::new(TaskState {
                listeners: Arc::new(RwLock::new(Listeners::default())),
                join_handles: [None, None, None],
            }),
            connect,
        })
    }

    pub(crate) fn connection(&self) -> &Arc<Mutex<RequestConnection>> {
        &self.connection
    }

    pub async fn add_workspace_listener(
        &self,
        listener: impl Fn(&WorkspaceEvent) + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Workspace(Arc::new(listener)))
            .await
    }

    pub async fn add_input_listener(
        &self,
        listener: impl Fn(&InputEvent) + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Input(Arc::new(listener))).await
    }

    pub async fn add_mode_listener(
        &self,
        listener: impl Fn(&ModeEvent) + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Mode(Arc::new(listener))).await
    }

    async fn add_listener(&self, listener: Listener) -> Result<()> {
        let mut state = self.task_state.lock().await;
        let event_type = listener.event_type();
        state
            .listeners
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(listener);

        // One durable subscription per event type is enough for every bar
        // output. Adding another listener of that type only extends the shared
        // callback list; it never tears down a healthy subscription.
        if state.join_handles[event_type.index()]
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return Ok(());
        }
        let event_types = vec![event_type];

        // Bound the initial handshake. Failure creates an idle supervisor for
        // this type; subscriptions for the other event types remain untouched.
        let connection = match connect_and_subscribe_with_timeout(
            &self.connect,
            &event_types,
            SUBSCRIPTION_ACK_TIMEOUT,
        )
        .await
        {
            Ok(connection) => Some(connection),
            Err(error) => {
                // Listener registration is local and durable for this Client.
                // A missing acknowledgement starts the same bounded reconnect
                // lifecycle used after EOF instead of hanging or panicking the
                // module that registered it.
                warn!(?error, "Sway IPC subscription will reconnect in background");
                None
            }
        };

        let shared_listeners = state.listeners.clone();
        state.join_handles[event_type.index()] = Some(spawn(subscription_loop(
            connection,
            event_types,
            shared_listeners,
            self.connect.clone(),
            RECONNECT_INITIAL,
            SUBSCRIPTION_ACK_TIMEOUT,
        )));
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        for handle in &mut self.task_state.get_mut().join_handles {
            if let Some(handle) = handle.take() {
                handle.abort();
            }
        }
    }
}

async fn subscription_loop(
    mut connection: Option<Connection>,
    event_types: Vec<EventType>,
    listeners: Arc<RwLock<Listeners>>,
    connect: Connector,
    initial_retry: Duration,
    acknowledgement_timeout: Duration,
) {
    let mut retry = initial_retry;
    loop {
        if let Some(active) = connection.as_mut() {
            match active.next_event().await {
                Ok(Some(event)) => {
                    trace!(?event, "Sway IPC event");
                    listeners
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .dispatch(&event);
                    retry = initial_retry;
                    continue;
                }
                Ok(None) => {
                    trace!("ignoring unsupported Sway IPC event");
                    continue;
                }
                Err(Error::InvalidJson { context, source }) => {
                    // The complete frame has already been consumed, so malformed
                    // data for one known event cannot desynchronise later frames.
                    warn!(?source, context, "ignoring malformed Sway IPC event");
                    continue;
                }
                Err(error) => warn!(?error, "Sway IPC subscription disconnected"),
            }
            drop(connection.take());
        }

        loop {
            tokio::time::sleep(retry).await;
            match connect_and_subscribe_with_timeout(
                &connect,
                &event_types,
                acknowledgement_timeout,
            )
            .await
            {
                Ok(next) => {
                    info!("Sway IPC subscription reconnected");
                    connection = Some(next);
                    retry = initial_retry;
                    break;
                }
                Err(error) => warn!(?error, "Sway IPC reconnect failed"),
            }
            retry = next_retry(retry);
        }
    }
}

fn next_retry(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_MAX)
}

fn sway_socket_path() -> Result<PathBuf> {
    std::env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or(Error::MissingSocket)
}

async fn transact<S>(stream: &mut S, message_type: u32, payload: &[u8]) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    write_frame(stream, message_type, payload).await?;
    let frame = read_frame(stream).await?;
    if frame.message_type != message_type {
        return Err(Error::UnexpectedMessageType {
            expected: message_type,
            actual: frame.message_type,
        });
    }
    Ok(frame.payload)
}

fn frame_header(payload_length: usize, message_type: u32) -> Result<[u8; HEADER_LEN]> {
    if payload_length > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge {
            length: payload_length,
            max: MAX_FRAME_BYTES,
        });
    }
    let payload_length = u32::try_from(payload_length).map_err(|_| Error::FrameTooLarge {
        length: payload_length,
        max: MAX_FRAME_BYTES,
    })?;
    let mut header = [0_u8; HEADER_LEN];
    header[..IPC_MAGIC.len()].copy_from_slice(IPC_MAGIC);
    header[6..10].copy_from_slice(&payload_length.to_le_bytes());
    header[10..14].copy_from_slice(&message_type.to_le_bytes());
    Ok(header)
}

async fn write_frame<W>(writer: &mut W, message_type: u32, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let header = frame_header(payload.len(), message_type)?;
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R>(reader: &mut R) -> Result<Frame>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    if &header[..IPC_MAGIC.len()] != IPC_MAGIC {
        return Err(Error::InvalidMagic);
    }
    let length = u32::from_le_bytes(header[6..10].try_into().expect("four-byte length")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge {
            length,
            max: MAX_FRAME_BYTES,
        });
    }
    let message_type =
        u32::from_le_bytes(header[10..14].try_into().expect("four-byte message type"));
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Frame {
        message_type,
        payload,
    })
}

fn decode_json<T>(payload: &[u8], context: &'static str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(payload).map_err(|source| Error::InvalidJson { context, source })
}

fn decode_event(frame: Frame) -> Result<Option<Event>> {
    match frame.message_type {
        WORKSPACE_EVENT => decode_json(&frame.payload, "workspace event")
            .map(Event::Workspace)
            .map(Some),
        INPUT_EVENT => decode_json(&frame.payload, "input event")
            .map(Event::Input)
            .map(Some),
        MODE_EVENT => decode_json(&frame.payload, "mode event")
            .map(Event::Mode)
            .map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncWriteExt, DuplexStream, duplex};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    fn encoded_frame(message_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = frame_header(payload.len(), message_type)
            .expect("fixture frame should fit")
            .to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    async fn acknowledge_subscription(server: &mut DuplexStream, expected: &[&str]) {
        let request = read_frame(server)
            .await
            .expect("subscription request should arrive");
        assert_eq!(request.message_type, SUBSCRIBE);
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&request.payload)
                .expect("subscription payload should be JSON"),
            expected
        );
        write_frame(server, SUBSCRIBE, br#"{"success":true}"#)
            .await
            .expect("subscription acknowledgement should write");
    }

    #[test]
    fn protocol_header_bytes_match_i3_wire_format() {
        assert_eq!(
            frame_header(3, GET_WORKSPACES).expect("small frame should fit"),
            [b'i', b'3', b'-', b'i', b'p', b'c', 3, 0, 0, 0, 1, 0, 0, 0,]
        );
        assert!(matches!(
            frame_header(MAX_FRAME_BYTES + 1, RUN_COMMAND),
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn fragmented_header_and_payload_are_reassembled() {
        let payload = br#"[{"id":7,"num":2,"name":"2:web","output":"DP-1"}]"#;
        let bytes = encoded_frame(GET_WORKSPACES, payload);
        let (mut writer, mut reader) = duplex(bytes.len());
        let task = tokio::spawn(async move {
            for chunk in bytes.chunks(3) {
                writer
                    .write_all(chunk)
                    .await
                    .expect("fragment should write");
                tokio::task::yield_now().await;
            }
        });

        let frame = read_frame(&mut reader)
            .await
            .expect("fragmented frame should decode");
        assert_eq!(frame.message_type, GET_WORKSPACES);
        assert_eq!(frame.payload, payload);
        task.await.expect("fragment writer should finish");
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_payload_allocation() {
        let mut header = frame_header(0, GET_INPUTS).expect("empty header should build");
        header[6..10].copy_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
        let (mut writer, mut reader) = duplex(HEADER_LEN);
        writer
            .write_all(&header)
            .await
            .expect("header should write");
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn run_command_preserves_exact_payload_and_reports_rejection() {
        let (client, mut server) = duplex(4096);
        let mut connection = Connection::from_io(client);
        let exact = "workspace name with spaces; $HOME %n".to_string();
        let expected = exact.clone();
        let task = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("command request should arrive");
            assert_eq!(request.message_type, RUN_COMMAND);
            assert_eq!(request.payload, expected.as_bytes());
            write_frame(
                &mut server,
                RUN_COMMAND,
                br#"[{"success":false,"parse_error":true,"error":"bad command"}]"#,
            )
            .await
            .expect("command rejection should write");
        });

        assert!(matches!(
            connection.run_command(exact).await,
            Err(Error::CommandRejected { reason }) if reason == "bad command"
        ));
        task.await.expect("command server should finish");
    }

    #[tokio::test]
    async fn malformed_and_wrong_type_command_responses_are_errors() {
        for (response_type, response, expected_wrong_type) in [
            (RUN_COMMAND, b"not-json".as_slice(), false),
            (GET_INPUTS, br#"[{"success":true}]"#.as_slice(), true),
        ] {
            let (client, mut server) = duplex(4096);
            let mut connection = Connection::from_io(client);
            let task = tokio::spawn(async move {
                let _ = read_frame(&mut server)
                    .await
                    .expect("command request should arrive");
                write_frame(&mut server, response_type, response)
                    .await
                    .expect("response should write");
            });
            let error = connection
                .run_command("nop".to_string())
                .await
                .expect_err("invalid response should fail");
            assert_eq!(
                matches!(error, Error::UnexpectedMessageType { .. }),
                expected_wrong_type
            );
            task.await.expect("response server should finish");
        }
    }

    #[tokio::test]
    async fn query_replies_decode_only_required_workspace_and_input_fields() {
        let (client, mut server) = duplex(8192);
        let mut connection = Connection::from_io(client);
        let task = tokio::spawn(async move {
            let workspaces = read_frame(&mut server)
                .await
                .expect("workspace request should arrive");
            assert_eq!(workspaces.message_type, GET_WORKSPACES);
            write_frame(
                &mut server,
                GET_WORKSPACES,
                br#"[{"id":42,"num":3,"name":"3","output":"eDP-1","visible":true,"focused":false,"rect":{"x":0}}]"#,
            )
            .await
            .expect("workspace response should write");

            let inputs = read_frame(&mut server)
                .await
                .expect("input request should arrive");
            assert_eq!(inputs.message_type, GET_INPUTS);
            write_frame(
                &mut server,
                GET_INPUTS,
                br#"[{"identifier":"1:2:keyboard","type":"keyboard","xkb_active_layout_name":"English (US)"}]"#,
            )
            .await
            .expect("input response should write");
        });

        assert_eq!(
            connection
                .get_workspaces()
                .await
                .expect("workspaces should decode")[0]
                .id,
            42
        );
        assert_eq!(
            connection.get_inputs().await.expect("inputs should decode")[0]
                .xkb_active_layout_name
                .as_deref(),
            Some("English (US)")
        );
        task.await.expect("query server should finish");
    }

    #[test]
    fn scroll_workspace_event_ignores_orientation_layouts_and_unknown_fields() {
        let frame = Frame {
            message_type: WORKSPACE_EVENT,
            payload: br#"{
                "change":"focus",
                "current":{"id":17,"num":4,"name":"4:web","output":"eDP-1","focused":true,"visible":true,"urgent":false,"layout":"horizontal","nodes":[]},
                "old":{"id":9,"num":2,"name":"2:term","output":"eDP-1","focused":false,"visible":true,"urgent":false,"layout":"vertical","floating_nodes":[]},
                "scroll_extension":{"future":true}
            }"#
            .to_vec(),
        };
        let Some(Event::Workspace(event)) = decode_event(frame).expect("event should decode")
        else {
            panic!("expected workspace event");
        };
        assert_eq!(event.change, WorkspaceChange::Focus);
        assert_eq!(event.current.expect("current workspace").id, 17);
        assert_eq!(event.old.expect("old workspace").id, 9);

        assert!(
            decode_event(Frame {
                message_type: EVENT_BIT | 127,
                payload: b"not even json".to_vec(),
            })
            .expect("unknown event should be ignored")
            .is_none()
        );
    }

    #[tokio::test]
    async fn subscription_reconnects_and_resubscribes_after_eof() {
        let (initial_client, mut initial_server) = duplex(8192);
        let (reconnect_client, mut reconnect_server) = duplex(8192);
        let mut initial = Connection::from_io(initial_client);

        let first_server = tokio::spawn(async move {
            acknowledge_subscription(&mut initial_server, &["workspace"]).await;
            write_frame(
                &mut initial_server,
                WORKSPACE_EVENT,
                br#"{"change":"init","current":{"id":1,"num":1,"name":"one","output":"DP-1"}}"#,
            )
            .await
            .expect("first event should write");
        });
        initial
            .subscribe(&[EventType::Workspace])
            .await
            .expect("initial subscription should succeed");

        let second_server = tokio::spawn(async move {
            acknowledge_subscription(&mut reconnect_server, &["workspace"]).await;
            write_frame(
                &mut reconnect_server,
                WORKSPACE_EVENT,
                br#"{"change":"init","current":{"id":2,"num":2,"name":"two","output":"DP-1"}}"#,
            )
            .await
            .expect("second event should write");
            std::future::pending::<()>().await;
        });

        let reconnects = Arc::new(StdMutex::new(VecDeque::from([Connection::from_io(
            reconnect_client,
        )])));
        let connector: Connector = {
            let reconnects = reconnects.clone();
            Arc::new(move || {
                let next = reconnects
                    .lock()
                    .expect("reconnect queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no test reconnect remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let mut listeners = Listeners::default();
        listeners.workspaces.push(Arc::new(move |event| {
            let _ = events_tx.send(
                event
                    .current
                    .as_ref()
                    .and_then(|workspace| workspace.name.clone())
                    .unwrap_or_default(),
            );
        }));
        let task = tokio::spawn(subscription_loop(
            Some(initial),
            vec![EventType::Workspace],
            Arc::new(RwLock::new(listeners)),
            connector,
            Duration::ZERO,
            Duration::from_secs(1),
        ));

        assert_eq!(
            timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .expect("first event timeout"),
            Some("one".to_string())
        );
        assert_eq!(
            timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .expect("reconnected event timeout"),
            Some("two".to_string())
        );

        task.abort();
        let _ = task.await;
        first_server.await.expect("first server should finish");
        second_server.abort();
        let _ = second_server.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_listeners_preserves_healthy_per_type_subscriptions() {
        let (request_client, _request_server) = duplex(1024);
        let (workspace_client, mut workspace_server) = duplex(4096);
        let (input_client, mut input_server) = duplex(4096);
        let connections = Arc::new(StdMutex::new(VecDeque::from([
            Connection::from_io(workspace_client),
            Connection::from_io(input_client),
        ])));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector: Connector = {
            let connections = connections.clone();
            let connect_count = connect_count.clone();
            Arc::new(move || {
                connect_count.fetch_add(1, Ordering::SeqCst);
                let next = connections
                    .lock()
                    .expect("listener connection queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no listener connection remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };
        let client = Client {
            connection: Arc::new(Mutex::new(RequestConnection::new(
                Connection::from_io(request_client),
                connector.clone(),
            ))),
            task_state: Mutex::new(TaskState {
                join_handles: [None, None, None],
                listeners: Arc::new(RwLock::new(Listeners::default())),
            }),
            connect: connector,
        };

        let (workspace_release_tx, workspace_release_rx) = tokio::sync::oneshot::channel();
        let workspace_task = tokio::spawn(async move {
            acknowledge_subscription(&mut workspace_server, &["workspace"]).await;
            workspace_release_rx
                .await
                .expect("workspace event release signal");
            write_frame(
                &mut workspace_server,
                WORKSPACE_EVENT,
                br#"{"change":"focus","current":{"id":17,"num":2,"name":"two","output":"DP-1"}}"#,
            )
            .await
            .expect("workspace event should write");
            std::future::pending::<()>().await;
        });
        let (input_release_tx, input_release_rx) = tokio::sync::oneshot::channel();
        let input_task = tokio::spawn(async move {
            acknowledge_subscription(&mut input_server, &["input"]).await;
            input_release_rx.await.expect("input event release signal");
            write_frame(
                &mut input_server,
                INPUT_EVENT,
                br#"{"change":"xkb_layout","input":{"identifier":"keyboard","xkb_active_layout_name":"US"}}"#,
            )
            .await
            .expect("input event should write");
            std::future::pending::<()>().await;
        });

        let (workspace_events_tx, mut workspace_events_rx) = mpsc::unbounded_channel();
        let first_tx = workspace_events_tx.clone();
        client
            .add_workspace_listener(move |event| {
                let _ = first_tx.send(("first", event.current.as_ref().map(|node| node.id)));
            })
            .await
            .expect("first workspace listener should register");
        client
            .add_workspace_listener(move |event| {
                let _ = workspace_events_tx
                    .send(("second", event.current.as_ref().map(|node| node.id)));
            })
            .await
            .expect("second workspace listener should share the subscription");

        let (input_events_tx, mut input_events_rx) = mpsc::unbounded_channel();
        client
            .add_input_listener(move |event| {
                let _ = input_events_tx.send(event.input.identifier.clone());
            })
            .await
            .expect("input listener should register independently");
        assert_eq!(connect_count.load(Ordering::SeqCst), 2);
        assert!(
            connections
                .lock()
                .expect("listener connection queue lock")
                .is_empty()
        );

        workspace_release_tx
            .send(())
            .expect("workspace event should be released");
        input_release_tx
            .send(())
            .expect("input event should be released");
        assert_eq!(
            timeout(Duration::from_secs(1), workspace_events_rx.recv())
                .await
                .expect("first shared workspace callback timeout"),
            Some(("first", Some(17)))
        );
        assert_eq!(
            timeout(Duration::from_secs(1), workspace_events_rx.recv())
                .await
                .expect("second shared workspace callback timeout"),
            Some(("second", Some(17)))
        );
        assert_eq!(
            timeout(Duration::from_secs(1), input_events_rx.recv())
                .await
                .expect("independent input callback timeout"),
            Some("keyboard".to_string())
        );

        drop(client);
        workspace_task.abort();
        input_task.abort();
        let _ = workspace_task.await;
        let _ = input_task.await;
    }

    #[tokio::test]
    async fn failed_request_is_not_retried_and_next_operation_reconnects() {
        let (first_client, mut first_server) = duplex(4096);
        let (second_client, mut second_server) = duplex(4096);
        let reconnects = Arc::new(StdMutex::new(VecDeque::from([Connection::from_io(
            second_client,
        )])));
        let connector: Connector = {
            let reconnects = reconnects.clone();
            Arc::new(move || {
                let next = reconnects
                    .lock()
                    .expect("request reconnect queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no request reconnect remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };
        let mut request = RequestConnection::new(Connection::from_io(first_client), connector);

        let first = tokio::spawn(async move {
            let command = read_frame(&mut first_server)
                .await
                .expect("first command should arrive exactly once");
            assert_eq!(command.message_type, RUN_COMMAND);
            assert_eq!(command.payload, b"exec maybe-started");
            // Losing the reply makes acceptance ambiguous. Closing this socket
            // must not cause the same command to be sent to the next one.
        });
        assert!(
            request
                .run_command_with_timeout(
                    "exec maybe-started".to_string(),
                    Duration::from_millis(100),
                )
                .await
                .is_err()
        );
        assert!(request.current.is_none());

        let second = tokio::spawn(async move {
            let next = read_frame(&mut second_server)
                .await
                .expect("next independent request should reconnect");
            assert_eq!(next.message_type, GET_INPUTS);
            write_frame(
                &mut second_server,
                GET_INPUTS,
                br#"[{"identifier":"keyboard","xkb_active_layout_name":"US"}]"#,
            )
            .await
            .expect("reconnected input response should write");
        });
        assert_eq!(
            request
                .get_inputs_with_timeout(Duration::from_millis(100))
                .await
                .expect("next operation should use a fresh connection")[0]
                .identifier,
            "keyboard"
        );
        first.await.expect("first request server should finish");
        second.await.expect("second request server should finish");
        assert!(
            reconnects
                .lock()
                .expect("request reconnect queue lock")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pending_connector_is_bounded_for_requests_and_subscriptions() {
        let connector: Connector = Arc::new(|| Box::pin(std::future::pending()));
        let mut request = RequestConnection {
            current: None,
            connect: connector.clone(),
        };
        assert!(matches!(
            request
                .get_inputs_with_timeout(Duration::from_millis(10))
                .await,
            Err(Error::RequestTimeout {
                operation: "input request",
                ..
            })
        ));
        assert!(request.current.is_none());

        assert!(matches!(
            connect_and_subscribe_with_timeout(
                &connector,
                &[EventType::Workspace],
                Duration::from_millis(10),
            )
            .await,
            Err(Error::RequestTimeout {
                operation: "subscription connection and acknowledgement",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn initial_subscription_acknowledgement_is_bounded() {
        let (client, mut server) = duplex(1024);
        let connections = Arc::new(StdMutex::new(VecDeque::from([Connection::from_io(client)])));
        let connector: Connector = {
            let connections = connections.clone();
            Arc::new(move || {
                let next = connections
                    .lock()
                    .expect("subscription connection queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no subscription connection remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };
        let server = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("subscription request should arrive");
            assert_eq!(request.message_type, SUBSCRIBE);
            std::future::pending::<()>().await;
        });
        assert!(matches!(
            connect_and_subscribe_with_timeout(
                &connector,
                &[EventType::Mode],
                Duration::from_millis(10),
            )
            .await,
            Err(Error::RequestTimeout { .. })
        ));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn rejected_subscription_never_starts_event_loop() {
        let (client, mut server) = duplex(1024);
        let mut connection = Connection::from_io(client);
        let task = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("subscription should arrive");
            assert_eq!(request.message_type, SUBSCRIBE);
            write_frame(
                &mut server,
                SUBSCRIBE,
                br#"{"success":false,"error":"unsupported event"}"#,
            )
            .await
            .expect("rejection should write");
        });
        assert!(matches!(
            connection.subscribe(&[EventType::Input]).await,
            Err(Error::SubscriptionRejected { reason }) if reason == "unsupported event"
        ));
        task.await.expect("subscription server should finish");
    }
}
