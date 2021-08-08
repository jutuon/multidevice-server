pub mod protocol;

use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream, tcp::{OwnedReadHalf, OwnedWriteHalf}}, sync::{mpsc::{self, Receiver, UnboundedReceiver}, Notify}, task::JoinHandle};
use tokio_stream::{Stream, StreamExt, wrappers::UnboundedReceiverStream};
use async_stream::stream;
use tokio_util::sync::CancellationToken;

use std::{collections::HashMap, convert::TryInto, io, num::TryFromIntError, sync::Arc};

use crate::logic::server::DMEvent;

use self::protocol::{ClientInfo, ClientMessage, ProtocolDeserializer, ProtocolDeserializerError, ServerInfo, ServerMessage};

use super::{CloseComponent, DMEventSender, ServerEvent, ServerEventSender};


#[derive(Debug)]
pub enum DeviceConnectionError {
    MessageSerializationError(serde_json::Error),
    MessageSendError(io::Error),
    MessageSendDataLengthError(TryFromIntError),
    MessageReceiveError(io::Error),
    MessageReceiveMessageSizeError,
    MessageReceiveProtocolDeserializeError(ProtocolDeserializerError),
}

#[derive(Debug)]
pub struct DeviceConnection {
    read_half: OwnedReadHalf,
    write_half: OwnedWriteHalf,
}

impl DeviceConnection {
    fn new(connection: TcpStream) -> Self {
        let (read_half, write_half) = connection.into_split();

        Self {
            read_half,
            write_half,
        }
    }

    pub async fn send_message(&mut self, message: ServerMessage) -> Result<(), DeviceConnectionError> {
        let data = serde_json::to_vec(&message)
            .map_err(DeviceConnectionError::MessageSerializationError)?;

        let data_len: i32 = data
            .len()
            .try_into()
            .map_err(DeviceConnectionError::MessageSendDataLengthError)?;

        let data_len = data_len as u32;

        self.write_half.write_all(&data_len.to_be_bytes())
            .await
            .map_err(DeviceConnectionError::MessageSendError)?;

        self.write_half.write_all(&data)
            .await
            .map_err(DeviceConnectionError::MessageSendError)
    }

    pub async fn receive_message(&mut self) -> Result<ClientMessage, DeviceConnectionError> {
        let message_len = self.read_half
            .read_u32()
            .await
            .map_err(DeviceConnectionError::MessageReceiveError)?;

        if message_len > i32::max_value() as u32 {
            return Err(DeviceConnectionError::MessageReceiveMessageSizeError);
        }

        let mut deserializer = ProtocolDeserializer::new();
        let message = deserializer
            .read_client_message(&mut self.read_half, message_len)
            .await
            .map_err(DeviceConnectionError::MessageReceiveProtocolDeserializeError)?;

        Ok(message)
    }
}


#[derive(Debug)]
pub enum DeviceError {
    DeviceConnectionError(DeviceConnectionError),
    UnknownFirstProtocolMessage,
}

#[derive(Debug)]
pub struct Device {
    device_connection: DeviceConnection,
    info: ClientInfo,
}

impl Device {
    pub async fn new(mut device_connection: DeviceConnection) -> Result<Self, DeviceError> {
        let message = ServerMessage::ServerInfo(ServerInfo::new("Test server"));
        device_connection.send_message(message).await.map_err(DeviceError::DeviceConnectionError)?;

        let message = device_connection.receive_message().await.map_err(DeviceError::DeviceConnectionError)?;

        let info = match message  {
            ClientMessage::ClientInfo(info) => info,
            _ => return Err(DeviceError::UnknownFirstProtocolMessage),
        };

        Ok(Device {
            device_connection,
            info,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.info.id
    }
}

#[derive(Debug)]
pub enum AcceptDeviceEvent {
    SocketListenerAcceptError(io::Error),
    DeviceConnectionInitError(DeviceError),
    NewDevice(Device),
}

#[derive(Debug)]
pub enum CreateTcpListenerEvent {
    ListenerCreationError(io::Error),
    ListenerCreated(TcpListener),
}

#[derive(Debug)]
pub enum DeviceManagerEvent {
    RequestQuit,
    Message(String),
    AcceptDeviceTaskEvent(AcceptDeviceEvent),
    CreateTcpListenerTaskEvent(CreateTcpListenerEvent),
}

#[derive(Debug, Clone)]
pub struct DeviceManagerEventSender {
    sender: mpsc::UnboundedSender<DeviceManagerEvent>,
}

impl DeviceManagerEventSender {
    pub fn new(sender: mpsc::UnboundedSender<DeviceManagerEvent>) -> Self {
        Self {
            sender,
        }
    }

    pub fn send(&mut self, event: DeviceManagerEvent) {
        self.sender.send(event).unwrap();
    }

    fn send_accept_device_event(&mut self, event: AcceptDeviceEvent) {
        self.send(DeviceManagerEvent::AcceptDeviceTaskEvent(event));
    }

    fn send_create_tcp_listener_event(&mut self, event: CreateTcpListenerEvent) {
        self.send(DeviceManagerEvent::CreateTcpListenerTaskEvent(event));
    }
}

#[derive(Debug)]
pub enum DeviceManagerError {
    SocketListenerCreationError(io::Error),
    SocketListenerAcceptError(io::Error),
}

enum DeviceManagerState {
    InitRunning {

    }
}


type DeviceId = String;

pub struct DeviceManagerAsync;

impl DeviceManagerAsync {
    async fn create_tcp_listener_task(
        cancellation_token: CancellationToken,
        mut dm_event_sender: DeviceManagerEventSender,
    ) {
        tokio::select! {
            _ = cancellation_token.cancelled() => return,
            bind_result = TcpListener::bind("127.0.0.1:8080") => {
                match bind_result {
                    Ok(listener) => {
                        dm_event_sender.send_create_tcp_listener_event(CreateTcpListenerEvent::ListenerCreated(listener));
                    }
                    Err(e) => {
                        dm_event_sender.send_create_tcp_listener_event(CreateTcpListenerEvent::ListenerCreationError(e));
                    }
                }
            }
        };
    }

    async fn accept_device_connection_task(
        listener: TcpListener,
        cancellation_token: CancellationToken,
        mut dm_event_sender: DeviceManagerEventSender,
    ) {
        loop {
            let socket = tokio::select! {
                _ = cancellation_token.cancelled() => return,
                listener_result = listener.accept() => {
                    match listener_result {
                        Ok((socket, _)) => socket,
                        Err(e) => {
                            dm_event_sender.send_accept_device_event(AcceptDeviceEvent::SocketListenerAcceptError(e));
                            return;
                        }
                    }
                }
            };

            tokio::select! {
                _ = cancellation_token.cancelled() => return,
                device_result = Device::new(DeviceConnection::new(socket)) => {
                    match device_result {
                        Ok(device) => {
                            dm_event_sender.send_accept_device_event(AcceptDeviceEvent::NewDevice(device));
                        }
                        Err(e) => {
                            dm_event_sender.send_accept_device_event(AcceptDeviceEvent::DeviceConnectionInitError(e));
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum DmTcpSupportDisabled {
    ListenerCreationError(io::Error),
}

pub enum DmTcpState {
    CreateTcpListener(JoinHandle<()>),
    AcceptNewConnections(JoinHandle<()>),
    Closed,
}

pub struct DmTcpStateManager {
    ct: CancellationToken,
    dm_event_sender: DeviceManagerEventSender,
    state: DmTcpState,
}

impl DmTcpStateManager {
    fn new(ct: CancellationToken, dm_event_sender: DeviceManagerEventSender) -> Self {
        let task = tokio::spawn(DeviceManagerAsync::create_tcp_listener_task(ct.child_token(), dm_event_sender.clone()));
        let state = DmTcpState::CreateTcpListener(task);
        Self {
            ct,
            dm_event_sender,
            state,
        }
    }

    async fn handle_create_tcp_listener_task_event(&mut self, event: CreateTcpListenerEvent, server_event_sender: &mut DMEventSender) {
        match &mut self.state {
            DmTcpState::CreateTcpListener(handle) => {
                match event {
                    CreateTcpListenerEvent::ListenerCreationError(e) => {
                        handle.await.unwrap();
                        let e = DmTcpSupportDisabled::ListenerCreationError(e);
                        server_event_sender.send(DMEvent::TcpSupportDisabledBecauseOfError(e));
                        self.state = DmTcpState::Closed;
                    }
                    CreateTcpListenerEvent::ListenerCreated(listener) => {
                        handle.await.unwrap();
                        let task = DeviceManagerAsync::accept_device_connection_task(
                            listener,
                            self.ct.child_token(),
                            self.dm_event_sender.clone(),
                        );
                        let new_handle = tokio::spawn(task);
                        self.state = DmTcpState::AcceptNewConnections(new_handle);
                    }
                }
            }
        _ => eprintln!("Warning: CreateTcpListenerEvent and state mismatch."),
        }

    }

    async fn handle_accept_device_task_event(&mut self, event: AcceptDeviceEvent, server_event_sender: &mut DMEventSender) -> Option<Device> {
        match &mut self.state {
            DmTcpState::AcceptNewConnections(handle) => {
                match event {
                    AcceptDeviceEvent::DeviceConnectionInitError(e) => {
                        eprintln!("DeviceConnectionInitError: {:?}", e);
                    }
                    AcceptDeviceEvent::SocketListenerAcceptError(e) => {
                        eprintln!("SocketListenerAcceptError: {:?}", e);
                    }
                    AcceptDeviceEvent::NewDevice(device) => {
                        return Some(device);
                    }
                }
            }
            _ => eprintln!("Warning: AcceptDeviceEvent and state mismatch."),
        }

        None
    }

    async fn wait_quit(&mut self) {
        match &mut self.state {
            DmTcpState::CreateTcpListener(handle) |
            DmTcpState::AcceptNewConnections(handle)  => {
                handle.await.unwrap();
                self.state = DmTcpState::Closed;
            }
            _ => (),
        }
    }

    fn closed(&self) -> bool {
        if let DmTcpState::Closed = self.state {
            true
        } else {
            false
        }
    }
}


pub struct DeviceManager {
    server_sender: DMEventSender,
    receiver: UnboundedReceiver<DeviceManagerEvent>,
    dm_event_sender: DeviceManagerEventSender,
    devices: HashMap<DeviceId, Device>,
    dm_tcp_state: DmTcpState,
}


impl DeviceManager {
    pub fn new(
        server_sender: DMEventSender,
        receiver: UnboundedReceiver<DeviceManagerEvent>,
        dm_event_sender: DeviceManagerEventSender
    ) -> Self {
        Self {
            server_sender,
            receiver,
            dm_event_sender,
            devices: HashMap::new(),
            dm_tcp_state: DmTcpState::Closed,
        }
    }

    pub async fn run(mut self) {
        let ct = CancellationToken::new();
        let mut tcp_state_manager = DmTcpStateManager::new(ct.child_token(), self.dm_event_sender.clone());

        loop {
            let event = self.receiver.recv().await.expect("Logic bug: server task channel broken.");
            match event {
                DeviceManagerEvent::Message(_) => {

                }
                DeviceManagerEvent::CreateTcpListenerTaskEvent(e) => {
                    tcp_state_manager.handle_create_tcp_listener_task_event(e, &mut self.server_sender).await;
                }
                DeviceManagerEvent::AcceptDeviceTaskEvent(e) => {
                    let device = tcp_state_manager.handle_accept_device_task_event(e, &mut self.server_sender).await;

                    if let Some(device) = device {
                        let id = device.device_id().to_string();
                        println!("New device connection. {:?}", device.info);
                        self.devices.insert(id, device);
                    }
                }
                DeviceManagerEvent::RequestQuit => {
                    ct.cancel();
                    tcp_state_manager.wait_quit().await;
                    // TODO: Close device connections.
                    self.server_sender.send(DMEvent::DMClosed);
                    break;
                }
            }
        }
    }


    pub fn create_device_event_channel() -> (DeviceManagerEventSender, UnboundedReceiver<DeviceManagerEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();

        (DeviceManagerEventSender::new(sender), receiver)
    }
}