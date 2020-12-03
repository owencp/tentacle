use futures::{
    channel::mpsc,
    prelude::*,
    stream::{FusedStream, StreamExt},
};
use log::{debug, error, log_enabled, trace};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::prelude::{AsyncRead, AsyncWrite};

#[cfg(not(target_arch = "wasm32"))]
use crate::service::helper::Listener;
use crate::{
    buffer::{Buffer, SendResult},
    channel::{mpsc as priority_mpsc, mpsc::Priority},
    context::{ServiceContext, SessionContext, SessionController},
    error::{DialerErrorKind, ListenErrorKind, ProtocolHandleErrorKind, TransportErrorKind},
    multiaddr::{Multiaddr, Protocol},
    protocol_handle_stream::{
        ServiceProtocolEvent, ServiceProtocolStream, SessionProtocolEvent, SessionProtocolStream,
    },
    protocol_select::ProtocolInfo,
    secio::{PublicKey, SecioKeyPair},
    service::{
        config::{ServiceConfig, State},
        event::ServiceTask,
        future_task::{BoxedFutureTask, FutureTaskManager},
        helper::{HandshakeContext, Source},
    },
    session::{Session, SessionEvent, SessionMeta},
    traits::ServiceHandle,
    transports::{MultiIncoming, MultiTransport, Transport},
    utils::extract_peer_id,
    yamux::Config as YamuxConfig,
    ProtocolId, SessionId,
};

pub(crate) mod config;
mod control;
pub(crate) mod event;
pub(crate) mod future_task;
mod helper;

pub use crate::service::{
    config::{BlockingFlag, ProtocolHandle, ProtocolMeta, TargetProtocol, TargetSession},
    control::{ServiceAsyncControl, ServiceControl},
    event::{ProtocolEvent, ServiceError, ServiceEvent},
    helper::SessionType,
};
use bytes::Bytes;

/// Received from user, aggregate mode
pub(crate) const RECEIVED_BUFFER_SIZE: usize = 2048;
/// Use to receive open/close event, no need too large
pub(crate) const RECEIVED_SIZE: usize = 512;
/// Send to remote, distribute mode
pub(crate) const SEND_SIZE: usize = 512;

type Result<T> = std::result::Result<T, TransportErrorKind>;

/// An abstraction of p2p service, currently only supports TCP/websocket protocol
pub struct Service<T> {
    protocol_configs: HashMap<ProtocolId, ProtocolMeta>,

    sessions: HashMap<SessionId, SessionController>,

    multi_transport: MultiTransport,

    listens: HashSet<Multiaddr>,

    #[cfg(not(target_arch = "wasm32"))]
    igd_client: Option<crate::upnp::IGDClient>,

    dial_protocols: HashMap<Multiaddr, TargetProtocol>,
    config: ServiceConfig,
    /// service state
    state: State,

    next_session: SessionId,

    before_sends: HashMap<ProtocolId, Box<dyn Fn(bytes::Bytes) -> bytes::Bytes + Send + 'static>>,

    /// Can be upgrade to list service level protocols
    handle: T,

    // Future task manager
    future_task_manager: Option<FutureTaskManager>,
    // To add a future task
    future_task_sender: Buffer<BoxedFutureTask>,

    service_proto_handles: HashMap<ProtocolId, Buffer<ServiceProtocolEvent>>,

    session_proto_handles: HashMap<(SessionId, ProtocolId), Buffer<SessionProtocolEvent>>,

    /// Send events to service, clone to session
    session_event_sender: mpsc::Sender<SessionEvent>,
    /// Receive event from service
    session_event_receiver: mpsc::Receiver<SessionEvent>,

    /// External event is passed in from this
    service_context: ServiceContext,
    /// External event receiver
    service_task_receiver: priority_mpsc::Receiver<ServiceTask>,

    shutdown: Arc<AtomicBool>,

    wait_handle: Vec<(
        Option<futures::channel::oneshot::Sender<()>>,
        crate::runtime::JoinHandle<()>,
    )>,
}

impl<T> Service<T>
where
    T: ServiceHandle + Unpin,
{
    /// New a Service
    pub(crate) fn new(
        protocol_configs: HashMap<ProtocolId, ProtocolMeta>,
        handle: T,
        key_pair: Option<SecioKeyPair>,
        forever: bool,
        config: ServiceConfig,
    ) -> Self {
        let (session_event_sender, session_event_receiver) = mpsc::channel(RECEIVED_SIZE);
        let (task_sender, task_receiver) = priority_mpsc::channel(RECEIVED_BUFFER_SIZE);
        let proto_infos = protocol_configs
            .values()
            .map(|meta| {
                let proto_info = ProtocolInfo::new(&meta.name(), meta.support_versions());
                (meta.id(), proto_info)
            })
            .collect();
        let (future_task_sender, future_task_receiver) = mpsc::channel(SEND_SIZE);
        let shutdown = Arc::new(AtomicBool::new(false));
        #[cfg(not(target_arch = "wasm32"))]
        let igd_client = if config.upnp {
            crate::upnp::IGDClient::new()
        } else {
            None
        };

        Service {
            protocol_configs,
            before_sends: HashMap::default(),
            handle,
            multi_transport: {
                #[allow(clippy::let_and_return)]
                let transport = MultiTransport::new(config.timeout).tcp_bind(config.tcp_bind_addr);
                #[cfg(feature = "ws")]
                let transport = transport.ws_bind(config.ws_bind_addr);
                transport
            },
            future_task_sender: Buffer::new(future_task_sender),
            future_task_manager: Some(FutureTaskManager::new(
                future_task_receiver,
                shutdown.clone(),
            )),
            sessions: HashMap::default(),
            service_proto_handles: HashMap::default(),
            session_proto_handles: HashMap::default(),
            listens: HashSet::new(),
            #[cfg(not(target_arch = "wasm32"))]
            igd_client,
            dial_protocols: HashMap::default(),
            state: State::new(forever),
            next_session: SessionId::default(),
            session_event_sender,
            session_event_receiver,
            service_context: ServiceContext::new(
                task_sender,
                proto_infos,
                key_pair,
                shutdown.clone(),
            ),
            config,
            service_task_receiver: task_receiver,
            shutdown,
            wait_handle: Vec::new(),
        }
    }

    /// Yamux config for service
    ///
    /// Panic when max_frame_length < yamux_max_window_size
    pub fn yamux_config(mut self, config: YamuxConfig) -> Self {
        assert!(self.config.max_frame_length as u32 >= config.max_stream_window_size);
        self.config.session_config.yamux_config = config;
        self
    }

    /// Secio max frame length
    ///
    /// Panic when max_frame_length < yamux_max_window_size
    pub fn max_frame_length(mut self, size: usize) -> Self {
        assert!(
            size as u32
                >= self
                    .config
                    .session_config
                    .yamux_config
                    .max_stream_window_size
        );
        self.config.max_frame_length = size;
        self
    }

    /// Listen on the given address.
    ///
    /// Return really listen multiaddr, but if use `/dns4/localhost/tcp/80`,
    /// it will return original value, and create a future task to DNS resolver later.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn listen(&mut self, address: Multiaddr) -> Result<Multiaddr> {
        let listen_future = self.multi_transport.listen(address.clone())?;

        match listen_future.await {
            Ok((addr, incoming)) => {
                let listen_address = addr.clone();

                self.handle.handle_event(
                    &mut self.service_context,
                    ServiceEvent::ListenStarted {
                        address: listen_address.clone(),
                    },
                );
                if let Some(client) = self.igd_client.as_mut() {
                    client.register(&listen_address)
                }
                self.listens.insert(listen_address.clone());

                self.spawn_listener(incoming, listen_address);

                Ok(addr)
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_listener(&mut self, incoming: MultiIncoming, listen_address: Multiaddr) {
        let listener = Listener {
            inner: incoming,
            key_pair: self.service_context.key_pair().cloned(),
            event_sender: self.session_event_sender.clone(),
            max_frame_length: self.config.max_frame_length,
            timeout: self.config.timeout,
            listen_addr: listen_address,
            future_task_sender: self.future_task_sender.clone_sender(),
        };
        let mut sender = self.future_task_sender.clone_sender();
        crate::runtime::spawn(async move {
            let res = sender
                .send(Box::pin(listener.for_each(|_| future::ready(()))))
                .await;
            if res.is_err() {
                trace!("spawn listener fail")
            }
        });
    }

    /// Use by inner
    #[cfg(not(target_arch = "wasm32"))]
    fn listen_inner(&mut self, address: Multiaddr) -> Result<()> {
        let listen_future = self.multi_transport.listen(address.clone())?;

        let mut sender = self.session_event_sender.clone();
        let task = async move {
            let result = listen_future.await;
            let event = match result {
                Ok((addr, incoming)) => SessionEvent::ListenStart {
                    listen_address: addr,
                    incoming,
                },
                Err(error) => SessionEvent::ListenError { address, error },
            };
            if let Err(err) = sender.send(event).await {
                error!("Listen address result send back error: {:?}", err);
            }
        };
        self.future_task_sender.push(Box::pin(task));
        self.state.increase();
        Ok(())
    }

    /// Dial the given address, doesn't actually make a request, just generate a future
    pub async fn dial(&mut self, address: Multiaddr, target: TargetProtocol) -> Result<&mut Self> {
        let dial_future = self.multi_transport.dial(address.clone())?;

        match dial_future.await {
            Ok((addr, incoming)) => {
                self.handshake(incoming, SessionType::Outbound, addr, None);
                self.dial_protocols.insert(address, target);
                self.state.increase();
                Ok(self)
            }
            Err(err) => Err(err),
        }
    }

    /// Use by inner
    #[inline(always)]
    fn dial_inner(&mut self, address: Multiaddr, target: TargetProtocol) -> Result<()> {
        self.dial_protocols.insert(address.clone(), target);
        let dial_future = self.multi_transport.dial(address.clone())?;

        let key_pair = self.service_context.key_pair().cloned();
        let timeout = self.config.timeout;
        let max_frame_length = self.config.max_frame_length;

        let mut sender = self.session_event_sender.clone();
        let task = async move {
            let result = dial_future.await;

            match result {
                Ok((addr, incoming)) => {
                    HandshakeContext {
                        ty: SessionType::Outbound,
                        remote_address: addr,
                        listen_address: None,
                        key_pair,
                        event_sender: sender,
                        max_frame_length,
                        timeout,
                    }
                    .handshake(incoming)
                    .await;
                }
                Err(error) => {
                    if let Err(err) = sender
                        .send(SessionEvent::DialError { address, error })
                        .await
                    {
                        error!("dial address result send back error: {:?}", err);
                    }
                }
            };
        };

        self.future_task_sender.push(Box::pin(task));
        self.state.increase();
        Ok(())
    }

    /// Get service current protocol configure
    pub fn protocol_configs(&self) -> &HashMap<ProtocolId, ProtocolMeta> {
        &self.protocol_configs
    }

    /// Get service control, control can send tasks externally to the runtime inside
    pub fn control(&self) -> &ServiceControl {
        self.service_context.control()
    }

    /// Distribute event to sessions
    #[inline]
    fn distribute_to_session(&mut self, cx: &mut Context) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }

        for control in self.sessions.values_mut() {
            if let SendResult::Pending = control.try_send(cx) {
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::SessionBlocked {
                        session_context: control.inner.clone(),
                    },
                );
            }
        }
    }

    /// Distribute event to user level
    #[inline(always)]
    fn distribute_to_user_level(&mut self, cx: &mut Context) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut error = false;

        for (proto_id, buffer) in self.service_proto_handles.iter_mut() {
            match buffer.try_send(cx) {
                SendResult::Pending => {
                    let error = ProtocolHandleErrorKind::Block(None);
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolHandleError {
                            proto_id: *proto_id,
                            error,
                        },
                    );
                }
                SendResult::Ok => (),
                SendResult::Disconnect => {
                    error = true;
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolHandleError {
                            proto_id: *proto_id,
                            error: ProtocolHandleErrorKind::AbnormallyClosed(None),
                        },
                    );
                }
            }
        }

        for ((session_id, proto_id), ref mut buffer) in self.session_proto_handles.iter_mut() {
            match buffer.try_send(cx) {
                SendResult::Pending => {
                    let error = ProtocolHandleErrorKind::Block(Some(*session_id));
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolHandleError {
                            proto_id: *proto_id,
                            error,
                        },
                    );
                }
                SendResult::Ok => (),
                SendResult::Disconnect => {
                    error = true;
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolHandleError {
                            proto_id: *proto_id,
                            error: ProtocolHandleErrorKind::AbnormallyClosed(Some(*session_id)),
                        },
                    )
                }
            }
        }

        if error {
            // if handle panic, close service
            self.handle_service_task(cx, ServiceTask::Shutdown(false), Priority::High);
        }
    }

    /// Spawn protocol handle
    #[inline]
    fn session_handles_open(
        &mut self,
        id: SessionId,
    ) -> Vec<(
        Option<futures::channel::oneshot::Sender<()>>,
        crate::runtime::JoinHandle<()>,
    )> {
        let mut handles = Vec::new();
        for (proto_id, meta) in self.protocol_configs.iter_mut() {
            if let ProtocolHandle::Callback(handle) | ProtocolHandle::Both(handle) =
                meta.session_handle()
            {
                if let Some(session_control) = self.sessions.get(&id) {
                    debug!("init session [{}] level proto [{}] handle", id, proto_id);
                    let (sender, receiver) = mpsc::channel(RECEIVED_SIZE);
                    self.session_proto_handles
                        .insert((id, *proto_id), Buffer::new(sender));

                    let stream = SessionProtocolStream::new(
                        handle,
                        self.service_context.clone_self(),
                        Arc::clone(&session_control.inner),
                        receiver,
                        (*proto_id, meta.blocking_flag()),
                        self.session_event_sender.clone(),
                        (
                            self.shutdown.clone(),
                            self.future_task_sender.clone_sender(),
                        ),
                    );
                    let (sender, receiver) = futures::channel::oneshot::channel();
                    let handle = crate::runtime::spawn(async move {
                        future::select(stream.for_each(|_| future::ready(())), receiver).await;
                    });
                    handles.push((Some(sender), handle));
                }
            } else {
                debug!("can't find proto [{}] session handle", proto_id);
            }
        }
        handles
    }

    fn handle_message(
        &mut self,
        cx: &mut Context,
        target: TargetSession,
        proto_id: ProtocolId,
        priority: Priority,
        data: Bytes,
    ) {
        let data = match self.before_sends.get(&proto_id) {
            Some(function) => function(data),
            None => data,
        };

        match target {
            // Send data to the specified protocol for the specified session.
            TargetSession::Single(id) => {
                if let Some(control) = self.sessions.get_mut(&id) {
                    control.push_message(proto_id, priority, data);
                }
            }
            // Send data to the specified protocol for the specified sessions.
            TargetSession::Multi(ids) => {
                for id in ids {
                    debug!(
                        "send message to session [{}], proto [{}], data len: {}",
                        id,
                        proto_id,
                        data.len()
                    );
                    if let Some(control) = self.sessions.get_mut(&id) {
                        control.push_message(proto_id, priority, data.clone())
                    }
                }
            }
            // Broadcast data for a specified protocol.
            TargetSession::All => {
                debug!(
                    "broadcast message, peer count: {}, proto_id: {}, data len: {}",
                    self.sessions.len(),
                    proto_id,
                    data.len()
                );
                for control in self.sessions.values_mut() {
                    control.push_message(proto_id, priority, data.clone())
                }
            }
        }
        self.distribute_to_session(cx);
    }

    /// Handshake
    #[inline]
    fn handshake<H>(
        &mut self,
        socket: H,
        ty: SessionType,
        remote_address: Multiaddr,
        listen_address: Option<Multiaddr>,
    ) where
        H: AsyncRead + AsyncWrite + Send + 'static + Unpin,
    {
        let handshake_task = HandshakeContext {
            ty,
            remote_address,
            listen_address,
            key_pair: self.service_context.key_pair().cloned(),
            event_sender: self.session_event_sender.clone(),
            max_frame_length: self.config.max_frame_length,
            timeout: self.config.timeout,
        }
        .handshake(socket);

        let mut future_task_sender = self.future_task_sender.clone_sender();

        crate::runtime::spawn(async move {
            if future_task_sender
                .send(Box::pin(handshake_task))
                .await
                .is_err()
            {
                trace!("handshake send err")
            }
        });
    }

    fn generate_next_session(&mut self) {
        loop {
            self.next_session = self.next_session.wrapping_add(1);
            if !self.sessions.contains_key(&self.next_session) {
                break;
            }
        }
    }

    fn reached_max_connection_limit(&self) -> bool {
        self.sessions
            .len()
            .checked_add(self.state.into_inner().unwrap_or_default())
            .map(|count| self.config.max_connection_number < count)
            .unwrap_or_default()
    }

    /// Session open
    #[inline]
    fn session_open<H>(
        &mut self,
        cx: &mut Context,
        mut handle: H,
        remote_pubkey: Option<PublicKey>,
        mut address: Multiaddr,
        ty: SessionType,
        listen_addr: Option<Multiaddr>,
    ) where
        H: AsyncRead + AsyncWrite + Send + 'static + Unpin,
    {
        let target = self
            .dial_protocols
            .remove(&address)
            .unwrap_or_else(|| TargetProtocol::All);
        if let Some(ref key) = remote_pubkey {
            // If the public key exists, the connection has been established
            // and then the useless connection needs to be closed.
            match self
                .sessions
                .values()
                .find(|&context| context.inner.remote_pubkey.as_ref() == Some(key))
            {
                Some(context) => {
                    trace!("Connected to the connected node");
                    if let Poll::Ready(Err(e)) = Pin::new(&mut handle).poll_shutdown(cx) {
                        trace!("handle poll shutdown err {}", e)
                    }
                    if ty.is_outbound() {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::DialerError {
                                error: DialerErrorKind::RepeatedConnection(context.inner.id),
                                address,
                            },
                        );
                    } else {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ListenError {
                                error: ListenErrorKind::RepeatedConnection(context.inner.id),
                                address: listen_addr.expect("listen address must exist"),
                            },
                        );
                    }
                    return;
                }
                None => {
                    // if peer id doesn't match return an error
                    if let Some(peer_id) = extract_peer_id(&address) {
                        if key.peer_id() != peer_id {
                            trace!("Peer id not match");
                            self.handle.handle_error(
                                &mut self.service_context,
                                ServiceError::DialerError {
                                    error: DialerErrorKind::PeerIdNotMatch,
                                    address,
                                },
                            );
                            return;
                        }
                    } else {
                        address.push(Protocol::P2P(Cow::Owned(key.peer_id().into_bytes())))
                    }
                }
            }
        }

        self.generate_next_session();

        let session_closed = Arc::new(AtomicBool::new(false));
        let pending_data_size = Arc::new(AtomicUsize::new(0));
        let (service_event_sender, service_event_receiver) = priority_mpsc::channel(SEND_SIZE);
        let session_control = SessionController::new(
            service_event_sender.clone(),
            Arc::new(SessionContext::new(
                self.next_session,
                address,
                ty,
                remote_pubkey,
                session_closed,
                pending_data_size,
            )),
        );

        let session_context = session_control.inner.clone();

        // must insert here, otherwise, the session protocol handle cannot be opened
        self.sessions
            .insert(session_control.inner.id, session_control);

        // Open all session protocol handles
        let handles = self.session_handles_open(self.next_session);

        let mut by_name = HashMap::with_capacity(self.protocol_configs.len());
        let mut by_id = HashMap::with_capacity(self.protocol_configs.len());
        self.protocol_configs.iter().for_each(|(key, value)| {
            by_name.insert(value.name(), value.inner.clone());
            by_id.insert(*key, value.inner.clone());
        });

        let meta = SessionMeta::new(
            self.config.timeout,
            session_context.clone(),
            service_event_sender,
            self.service_context.control().clone(),
        )
        .protocol_by_name(by_name)
        .protocol_by_id(by_id)
        .config(self.config.session_config)
        .keep_buffer(self.config.keep_buffer)
        .service_proto_senders(self.service_proto_handles.clone())
        .session_senders(
            self.session_proto_handles
                .iter()
                .filter_map(|((session_id, key), value)| {
                    if *session_id == self.next_session {
                        Some((*key, value.clone()))
                    } else {
                        None
                    }
                })
                .collect(),
        )
        .session_proto_handles(handles)
        .event(self.config.event.clone());

        let mut session = Session::new(
            handle,
            self.session_event_sender.clone(),
            service_event_receiver,
            meta,
            self.future_task_sender.clone_sender(),
        );

        if ty.is_outbound() {
            match target {
                TargetProtocol::All => {
                    self.protocol_configs
                        .values()
                        .for_each(|meta| session.open_proto_stream(&meta.name()));
                }
                TargetProtocol::Single(proto_id) => {
                    if let Some(meta) = self.protocol_configs.get(&proto_id) {
                        session.open_proto_stream(&meta.name());
                    }
                }
                TargetProtocol::Multi(proto_ids) => proto_ids.into_iter().for_each(|id| {
                    if let Some(meta) = self.protocol_configs.get(&id) {
                        session.open_proto_stream(&meta.name());
                    }
                }),
            }
        }

        crate::runtime::spawn(session.for_each(|_| future::ready(())));

        self.handle.handle_event(
            &mut self.service_context,
            ServiceEvent::SessionOpen { session_context },
        );
    }

    /// Close the specified session, clean up the handle
    #[inline]
    fn session_close(&mut self, cx: &mut Context, id: SessionId, source: Source) {
        if source == Source::External {
            if let Some(control) = self.sessions.get_mut(&id) {
                control.push(Priority::High, SessionEvent::SessionClose { id });
                debug!("try close service session [{}] ", id);
                self.distribute_to_session(cx);
            }
            return;
        }

        debug!("close service session [{}]", id);

        // clean session proto handles sender
        self.session_proto_handles.retain(|key, _| id != key.0);

        if let Some(session_control) = self.sessions.remove(&id) {
            // Service handle processing flow
            self.handle.handle_event(
                &mut self.service_context,
                ServiceEvent::SessionClose {
                    session_context: session_control.inner,
                },
            );
        }
    }

    /// Open the handle corresponding to the protocol
    #[inline]
    fn protocol_open(
        &mut self,
        cx: &mut Context,
        id: SessionId,
        proto_id: ProtocolId,
        version: String,
        source: Source,
    ) {
        if source == Source::External {
            if let Some(control) = self.sessions.get_mut(&id) {
                control.push(
                    Priority::High,
                    SessionEvent::ProtocolOpen {
                        id,
                        proto_id,
                        version,
                    },
                );
                debug!("try open session [{}] proto [{}]", id, proto_id);
                self.distribute_to_session(cx);
            }
            return;
        }

        debug!("service session [{}] proto [{}] open", id, proto_id);

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&id) {
                // event output
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Connected {
                        session_context: Arc::clone(&session_control.inner),
                        proto_id,
                        version,
                    },
                );
            }
        }
    }

    /// Processing the received data
    #[inline]
    fn protocol_message(
        &mut self,
        session_id: SessionId,
        proto_id: ProtocolId,
        data: bytes::Bytes,
    ) {
        debug!(
            "service receive session [{}] proto [{}] data len: {}",
            session_id,
            proto_id,
            data.len()
        );

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&session_id) {
                // event output
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Received {
                        session_context: Arc::clone(&session_control.inner),
                        proto_id,
                        data,
                    },
                );
            }
        }
    }

    /// Protocol stream is closed, clean up data
    #[inline]
    fn protocol_close(
        &mut self,
        cx: &mut Context,
        session_id: SessionId,
        proto_id: ProtocolId,
        source: Source,
    ) {
        if source == Source::External {
            if let Some(control) = self.sessions.get_mut(&session_id) {
                control.push(
                    Priority::High,
                    SessionEvent::ProtocolClose {
                        id: session_id,
                        proto_id,
                    },
                );
                debug!("try close session [{}] proto [{}]", session_id, proto_id);
                self.distribute_to_session(cx);
            }
            return;
        }

        debug!(
            "service session [{}] proto [{}] close",
            session_id, proto_id
        );

        if self.config.event.contains(&proto_id) {
            if let Some(session_control) = self.sessions.get(&session_id) {
                self.handle.handle_proto(
                    &mut self.service_context,
                    ProtocolEvent::Disconnected {
                        proto_id,
                        session_context: Arc::clone(&session_control.inner),
                    },
                )
            }
        }
        self.session_proto_handles.remove(&(session_id, proto_id));
    }

    fn send_pending_task(&mut self, cx: &mut Context) {
        self.future_task_sender.try_send(cx);
    }

    #[inline]
    fn send_future_task(&mut self, cx: &mut Context, task: BoxedFutureTask) {
        self.future_task_sender.push(task);
        self.send_pending_task(cx)
    }

    fn init_proto_handles(&mut self) {
        for (proto_id, meta) in self.protocol_configs.iter_mut() {
            if let ProtocolHandle::Callback(handle) | ProtocolHandle::Both(handle) =
                meta.service_handle()
            {
                debug!("init service level [{}] proto handle", proto_id);
                let (sender, receiver) = mpsc::channel(RECEIVED_SIZE);
                self.service_proto_handles
                    .insert(*proto_id, Buffer::new(sender));

                let mut stream = ServiceProtocolStream::new(
                    handle,
                    self.service_context.clone_self(),
                    receiver,
                    (*proto_id, meta.blocking_flag()),
                    self.session_event_sender.clone(),
                    (
                        self.shutdown.clone(),
                        self.future_task_sender.clone_sender(),
                    ),
                );
                stream.handle_event(ServiceProtocolEvent::Init);
                let (sender, receiver) = futures::channel::oneshot::channel();
                let handle = crate::runtime::spawn(async move {
                    future::select(stream.for_each(|_| future::ready(())), receiver).await;
                });
                self.wait_handle.push((Some(sender), handle));
            } else {
                debug!("can't find proto [{}] service handle", proto_id);
            }
            if let Some(function) = meta.before_send.take() {
                self.before_sends.insert(*proto_id, function);
            }
        }
    }

    /// When listen update, call here
    #[cfg(not(target_arch = "wasm32"))]
    #[inline]
    fn try_update_listens(&mut self, cx: &mut Context) {
        if let Some(client) = self.igd_client.as_mut() {
            client.process_only_leases_support()
        }
        if self.listens.len() == self.service_context.listens().len() {
            return;
        }
        let new_listens = self.listens.iter().cloned().collect::<Vec<Multiaddr>>();
        self.service_context.update_listens(new_listens.clone());

        for buffer in self.service_proto_handles.values_mut() {
            buffer.push(ServiceProtocolEvent::Update {
                listen_addrs: new_listens.clone(),
            });
        }

        for buffer in self.session_proto_handles.values_mut() {
            buffer.push(SessionProtocolEvent::Update {
                listen_addrs: new_listens.clone(),
            });
        }

        self.distribute_to_user_level(cx);
    }

    /// Handling various events uploaded by the session
    fn handle_session_event(&mut self, cx: &mut Context, event: SessionEvent) {
        match event {
            SessionEvent::SessionClose { id } => self.session_close(cx, id, Source::Internal),
            SessionEvent::HandshakeSuccess {
                handle,
                public_key,
                address,
                ty,
                listen_address,
            } => {
                if ty.is_outbound() {
                    self.state.decrease();
                }
                if !self.reached_max_connection_limit() {
                    self.session_open(cx, handle, public_key, address, ty, listen_address);
                }
            }
            SessionEvent::HandshakeError { ty, error, address } => {
                if ty.is_outbound() {
                    self.state.decrease();
                    self.dial_protocols.remove(&address);
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::DialerError {
                            address,
                            error: DialerErrorKind::HandshakeError(error),
                        },
                    )
                }
            }
            SessionEvent::ProtocolMessage {
                id, proto_id, data, ..
            } => self.protocol_message(id, proto_id, data),
            SessionEvent::ProtocolOpen {
                id,
                proto_id,
                version,
                ..
            } => self.protocol_open(cx, id, proto_id, version, Source::Internal),
            SessionEvent::ProtocolClose { id, proto_id } => {
                self.protocol_close(cx, id, proto_id, Source::Internal)
            }
            SessionEvent::ProtocolSelectError { id, proto_name } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ProtocolSelectError {
                            proto_name,
                            session_context: Arc::clone(&session_control.inner),
                        },
                    )
                }
            }
            SessionEvent::ProtocolError {
                id,
                proto_id,
                error,
            } => self.handle.handle_error(
                &mut self.service_context,
                ServiceError::ProtocolError {
                    id,
                    proto_id,
                    error,
                },
            ),
            SessionEvent::DialError { address, error } => {
                self.state.decrease();
                self.dial_protocols.remove(&address);
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::DialerError {
                        address,
                        error: DialerErrorKind::TransportError(error),
                    },
                )
            }
            #[cfg(not(target_arch = "wasm32"))]
            SessionEvent::ListenError { address, error } => {
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::ListenError {
                        address: address.clone(),
                        error: ListenErrorKind::TransportError(error),
                    },
                );
                if self.listens.remove(&address) {
                    if let Some(ref mut client) = self.igd_client {
                        client.remove(&address);
                    }

                    self.handle.handle_event(
                        &mut self.service_context,
                        ServiceEvent::ListenClose { address },
                    )
                } else {
                    // try start listen error
                    self.state.decrease();
                }
            }
            SessionEvent::SessionTimeout { id } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::SessionTimeout {
                            session_context: Arc::clone(&session_control.inner),
                        },
                    )
                }
            }
            SessionEvent::MuxerError { id, error } => {
                if let Some(session_control) = self.sessions.get(&id) {
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::MuxerError {
                            session_context: Arc::clone(&session_control.inner),
                            error,
                        },
                    )
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            SessionEvent::ListenStart {
                listen_address,
                incoming,
            } => {
                self.handle.handle_event(
                    &mut self.service_context,
                    ServiceEvent::ListenStarted {
                        address: listen_address.clone(),
                    },
                );
                self.listens.insert(listen_address.clone());
                self.state.decrease();
                self.try_update_listens(cx);
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(client) = self.igd_client.as_mut() {
                    client.register(&listen_address)
                }
                self.spawn_listener(incoming, listen_address);
            }
            SessionEvent::ProtocolHandleError { error, proto_id } => {
                self.handle.handle_error(
                    &mut self.service_context,
                    ServiceError::ProtocolHandleError { error, proto_id },
                );
                // if handle panic, close service
                self.handle_service_task(cx, ServiceTask::Shutdown(false), Priority::High);
            }
            _ => (),
        }
    }

    /// Handling various tasks sent externally
    fn handle_service_task(&mut self, cx: &mut Context, event: ServiceTask, priority: Priority) {
        match event {
            ServiceTask::ProtocolMessage {
                target,
                proto_id,
                data,
            } => {
                self.handle_message(cx, target, proto_id, priority, data);
            }
            ServiceTask::Dial { address, target } => {
                if !self.dial_protocols.contains_key(&address) {
                    if let Err(e) = self.dial_inner(address.clone(), target) {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::DialerError {
                                address,
                                error: DialerErrorKind::TransportError(e),
                            },
                        );
                    }
                }
            }
            ServiceTask::Listen { address } =>
            {
                #[cfg(not(target_arch = "wasm32"))]
                if !self.listens.contains(&address) {
                    if let Err(e) = self.listen_inner(address.clone()) {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ListenError {
                                address,
                                error: ListenErrorKind::TransportError(e),
                            },
                        );
                    }
                }
            }
            ServiceTask::Disconnect { session_id } => {
                self.session_close(cx, session_id, Source::External)
            }
            ServiceTask::FutureTask { task } => {
                self.send_future_task(cx, task);
            }
            ServiceTask::SetProtocolNotify {
                proto_id,
                interval,
                token,
            } => {
                // TODO: if not contains should call handle_error let user know
                if let Some(buffer) = self.service_proto_handles.get_mut(&proto_id) {
                    buffer.push(ServiceProtocolEvent::SetNotify { interval, token });
                    self.distribute_to_user_level(cx);
                }
            }
            ServiceTask::RemoveProtocolNotify { proto_id, token } => {
                if let Some(buffer) = self.service_proto_handles.get_mut(&proto_id) {
                    buffer.push(ServiceProtocolEvent::RemoveNotify { token });
                    self.distribute_to_user_level(cx);
                }
            }
            ServiceTask::SetProtocolSessionNotify {
                session_id,
                proto_id,
                interval,
                token,
            } => {
                // TODO: if not contains should call handle_error let user know
                if let Some(buffer) = self.session_proto_handles.get_mut(&(session_id, proto_id)) {
                    buffer.push(SessionProtocolEvent::SetNotify { interval, token });
                    self.distribute_to_user_level(cx);
                }
            }
            ServiceTask::RemoveProtocolSessionNotify {
                session_id,
                proto_id,
                token,
            } => {
                if let Some(buffer) = self.session_proto_handles.get_mut(&(session_id, proto_id)) {
                    buffer.push(SessionProtocolEvent::RemoveNotify { token });
                    self.distribute_to_user_level(cx)
                }
            }
            ServiceTask::ProtocolOpen { session_id, target } => match target {
                TargetProtocol::All => {
                    // Borrowed check attack
                    #[allow(clippy::needless_collect)]
                    {
                        let ids = self.protocol_configs.keys().copied().collect::<Vec<_>>();
                        ids.into_iter().for_each(|id| {
                            self.protocol_open(
                                cx,
                                session_id,
                                id,
                                String::default(),
                                Source::External,
                            )
                        });
                    }
                }
                TargetProtocol::Single(id) => {
                    self.protocol_open(cx, session_id, id, String::default(), Source::External)
                }
                TargetProtocol::Multi(ids) => ids.into_iter().for_each(|id| {
                    self.protocol_open(cx, session_id, id, String::default(), Source::External)
                }),
            },
            ServiceTask::ProtocolClose {
                session_id,
                proto_id,
            } => self.protocol_close(cx, session_id, proto_id, Source::External),
            ServiceTask::Shutdown(quick) => {
                self.state.pre_shutdown();

                for address in self.listens.drain() {
                    self.handle.handle_event(
                        &mut self.service_context,
                        ServiceEvent::ListenClose { address },
                    )
                }
                // clear upnp register
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(client) = self.igd_client.as_mut() {
                    client.clear()
                };
                self.future_task_sender.clear();

                let sessions = self.sessions.keys().cloned().collect::<Vec<SessionId>>();

                if quick {
                    self.service_task_receiver.close();
                    self.session_event_receiver.close();
                    // clean buffer
                    self.service_proto_handles.clear();
                    self.session_proto_handles.clear();

                    // don't care about any session action
                    sessions
                        .into_iter()
                        .for_each(|i| self.session_close(cx, i, Source::Internal));
                } else {
                    sessions
                        .into_iter()
                        .for_each(|i| self.session_close(cx, i, Source::External));
                }
            }
        }
    }

    #[inline]
    fn user_task_poll(&mut self, cx: &mut Context) -> Poll<Option<()>> {
        if self
            .sessions
            .values()
            .map(|item| item.buffer.len())
            .sum::<usize>()
            > self.config.session_config.send_event_size()
        {
            // The write buffer exceeds the expected range, and no longer receives any event
            // from the user, This means that the session handle events is too slow, and each time
            // the sessions processes a event, the service is notified that it can receive
            // another event.
            return Poll::Pending;
        }

        if self.service_task_receiver.is_terminated() {
            return Poll::Ready(None);
        }

        match Pin::new(&mut self.service_task_receiver)
            .as_mut()
            .poll_next(cx)
        {
            Poll::Ready(Some((priority, task))) => {
                self.handle_service_task(cx, task, priority);
                Poll::Ready(Some(()))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn session_poll(&mut self, cx: &mut Context) -> Poll<Option<()>> {
        if self
            .service_proto_handles
            .values()
            .map(Buffer::len)
            .sum::<usize>()
            > self.config.session_config.recv_event_size()
            || self
                .session_proto_handles
                .values()
                .map(Buffer::len)
                .sum::<usize>()
                > self.config.session_config.recv_event_size()
        {
            // The read buffer exceeds the expected range, and no longer receives any event
            // from the sessions, This means that the user's handle processing is too slow, and
            // each time the user processes a event, the service is notified that it can receive
            // another event.
            return Poll::Pending;
        }

        if self.session_event_receiver.is_terminated() {
            return Poll::Ready(None);
        }

        match Pin::new(&mut self.session_event_receiver)
            .as_mut()
            .poll_next(cx)
        {
            Poll::Ready(Some(event)) => {
                self.handle_session_event(cx, event);
                Poll::Ready(Some(()))
            }
            Poll::Ready(None) => unreachable!(),
            Poll::Pending => Poll::Pending,
        }
    }

    fn flush_buffer(&mut self, cx: &mut Context) {
        if !self.sessions.values().all(|con| con.buffer.is_empty()) {
            self.distribute_to_session(cx);
        }
        if !self
            .service_proto_handles
            .values()
            .all(|buffer| buffer.is_empty())
            || !self
                .session_proto_handles
                .values()
                .all(|buffer| buffer.is_empty())
        {
            self.distribute_to_user_level(cx);
        }
    }

    #[cold]
    fn wait_handle_poll(&mut self, cx: &mut Context) -> Poll<Option<()>> {
        for (sender, mut handle) in self.wait_handle.split_off(0) {
            if let Some(sender) = sender {
                // don't care about it
                let _ignore = sender.send(());
            }
            match handle.poll_unpin(cx) {
                Poll::Pending => {
                    self.wait_handle.push((None, handle));
                }
                Poll::Ready(_) => (),
            }
        }

        if self.wait_handle.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

impl<T> Stream for Service<T>
where
    T: ServiceHandle + Unpin,
{
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if self.listens.is_empty()
            && self.state.is_shutdown()
            && self.sessions.is_empty()
            && self.future_task_sender.is_empty()
        {
            debug!("shutdown because all state is empty head");
            self.shutdown.store(true, Ordering::SeqCst);
            return self.wait_handle_poll(cx);
        }

        if let Some(stream) = self.future_task_manager.take() {
            let (sender, receiver) = futures::channel::oneshot::channel();
            let handle = crate::runtime::spawn(async move {
                future::select(stream.for_each(|_| future::ready(())), receiver).await;
            });
            self.wait_handle.push((Some(sender), handle));
            self.init_proto_handles();
        }

        self.flush_buffer(cx);

        #[cfg(not(target_arch = "wasm32"))]
        self.try_update_listens(cx);

        let mut is_pending = self.session_poll(cx).is_pending();

        // receive user task
        is_pending &= self.user_task_poll(cx).is_pending();

        // process any task buffer
        self.send_pending_task(cx);

        // Double check service state
        if self.listens.is_empty()
            && self.state.is_shutdown()
            && self.sessions.is_empty()
            && self.future_task_sender.is_empty()
        {
            debug!("shutdown because all state is empty tail");
            self.shutdown.store(true, Ordering::SeqCst);
            return self.wait_handle_poll(cx);
        }

        if log_enabled!(log::Level::Debug) {
            debug!(
                "listens count: {}, state: {:?}, sessions count: {}, \
             pending task: {}, write_buf: {}, read_service_buf: {}, read_session_buf: {}",
                self.listens.len(),
                self.state,
                self.sessions.len(),
                self.future_task_sender.len(),
                self.sessions
                    .values()
                    .map(|item| item.buffer.len())
                    .sum::<usize>(),
                self.service_proto_handles
                    .values()
                    .map(Buffer::len)
                    .sum::<usize>(),
                self.session_proto_handles
                    .values()
                    .map(Buffer::len)
                    .sum::<usize>(),
            );
        }

        if is_pending {
            Poll::Pending
        } else {
            Poll::Ready(Some(()))
        }
    }
}
