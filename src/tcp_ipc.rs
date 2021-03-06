use super::protocol_buffer::*;

pub use super::protocol_buffer::{ParseHeaderError, Protocol};
use log::*;
use mio::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::sync::mpsc::TryRecvError;

const BUFFER_SIZE: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
/// This bundles the time-settings for the protocol
/// A 'None' value means that there will no time spend waiting.
/// # Example
/// ```
/// let config = TcpIpcConfig {
///     after_connect_wait_time: Some(std::time::Duration::from_micros(5_000)),
///     read_iteration_wait_time: Some(std::time::Duration::from_micros(1)),
///     shutdown_wait_time: Some(std::time::Duration::from_micros(5_000_000)),
///     check_count: 1,
/// };
/// ```
pub struct TcpIpcConfig {
    /// This is the time the program waits for the server after it accepted the initial TCP connection.
    /// For example, this can be used to wait for the server doing some initialization.
    /// Moreover, the message read queue thread needs some time to start.
    pub after_connect_wait_time: Option<std::time::Duration>,
    /// This is the time the client sleeps between checking for new messages from the server.
    /// Very small values can yield high CPU-usage.
    pub read_iteration_wait_time: Option<std::time::Duration>,
    /// This is the time the client waits for the server to accept a shutdown request.
    pub shutdown_wait_time: Option<std::time::Duration>,
    /// This is the number of iterations inside the read thread after which the busy_update_state will be checked
    /// A good default value is 1 (check after each iteration)
    pub check_count: u32,
}

#[derive(Debug)]
enum ReadThreadErrorsInternal<P: Protocol> {
    WriteError(std::io::Error),
    ReadError(std::io::Error),
    ImmediateMessageConstructError((P::Commands, Vec<u8>)),
}
#[derive(Debug)]
/// The error type for operations in the asynchronous read thread
pub enum ReadThreadErrors<P: Protocol> {
    /// This indicates that a immediate respond in the read-thread failed
    WriteError(std::io::Error),
    /// This indicates that the read-thread failed to receive a message
    ReadError(std::io::Error),
    /// This indicates that the read-thread failed to construct a message.
    /// This typically happens if the protocol implementation has a flaw.
    ImmediateMessageConstructError((P::Commands, Vec<u8>)),
    /// This happens if the read-thread is disconnected from the server.
    Disconnected,
}
/// The error type for the connect-function.
#[derive(Debug)]
pub enum ConnectErrors {
    /// This happens if the input socket list is not a valid address.
    /// For example, the port may be missing.
    SocketListParseError(std::io::Error),
    /// The parsed socket list is empty
    SocketListIsEmpty,
    /// This occurs if the server is not available during connecting.
    ConnectionError(std::io::Error),
    /// This happens if a connection was established succesfully,
    /// but the cloning of the streams for the asynchronous read thread failed.
    TryCloneError(std::io::Error),
    /// This happens if a server tries to bind a socket address and fails.
    BindError(std::io::Error),
    /// Internally the tcp-stream is set to NoDelay (as default).
    /// This error indicates that this operation failed.
    SetNodelayError(std::io::Error),
    /// Internally the tcp-stream receive buffer size is set to header length.
    /// This error indicates that this operation failed.
    SetReceiveBufferSizeError(std::io::Error),
    /// Internally the tcp-stream send buffer size is set to header length.
    /// This error indicates that this operation failed.
    SetSendBufferSizeError(std::io::Error),
    /// This error indicates that the given wait time was exceeded
    WaitTimeExceeded,
}
/// This is the main type of the library.
/// Here all the logic is bundle.
/// It can be used to easily send and receive messages via TCP, allowing for many different protcols to be used.
pub struct TcpIpc<P: Protocol> {
    busy_state_sender: std::sync::mpsc::Sender<P::BusyStates>,
    message_receiver: std::sync::mpsc::Receiver<Result<Message<P>, ReadThreadErrorsInternal<P>>>,
    stream: TcpStream,
    shutdown_sender: std::sync::mpsc::Sender<()>,
    shutdown_wait_time: Option<std::time::Duration>,
    busy_state_query_sender: std::sync::mpsc::Sender<()>,
    busy_state_queried_receiver: std::sync::mpsc::Receiver<P::BusyStates>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// The error type for a BusyState update
pub enum BusyStateUpdateResult {
    /// Update succesful
    Success,
    /// The only posibility for fail is that the connection is already (disgracefully) closed.
    Disconnected,
}
#[derive(Debug, Clone, Copy, PartialEq)]
/// The error type for a BusyState query
pub enum BusyStateQueryResult {
    /// The only posibility for fail is that the connection is already (disgracefully) closed.
    Disconnected,
}
#[derive(Debug)]
/// The error type for a message writing
pub enum WriteMessageErrors {
    /// Failed to construct message. This indicates that the protocol implementation has a flaw
    MessageConstructionFailed,
    /// Failed to send message.
    /// This indicates typically a run-time problem.
    MessageSendFailed(std::io::Error),
}
impl<P: Protocol> TcpIpc<P> {
    /// This connects a client to a server, allowing to send and receive commands.
    /// The input variable 'connect_wait_time' is the time the client waits for the Server to accept a TCP-connection.
    /// A 'None' value yields an infinite waiting period.
    /// # Example
    /// ```
    /// let config = TcpIpcConfig {
    ///     connect_wait_time_ms: 5_000,
    ///     read_iteration_wait_time_ns: 1_000,
    ///     shutdown_wait_time_in_ns: 1_000_000,
    /// };
    /// let mut client =
    ///     TcpIpc::<ProtocolExample>::client("127.0.0.1:6666", config, None).expect("connecting failed");
    /// ```
    pub fn client<T: ToSocketAddrs>(
        socket_addresses: T,
        config: TcpIpcConfig,
        connect_wait_time: Option<std::time::Duration>,
    ) -> Result<TcpIpc<P>, ConnectErrors> {
        // connect
        let client = {
            let mut error = self::ConnectErrors::SocketListIsEmpty;
            let mut socket_addresses = socket_addresses
                .to_socket_addrs()
                .map_err(ConnectErrors::SocketListParseError)?;
            loop {
                if let Some(socket_address) = socket_addresses.next() {
                    debug!("trying to connect to {:?}", socket_address);
                    match if let Some(connect_wait_time) = connect_wait_time {
                        let now = std::time::Instant::now();
                        loop {
                            match TcpStream::connect(&socket_address) {
                                Ok(stream) => break Ok(stream),
                                Err(error) => match error.kind() {
                                    std::io::ErrorKind::WouldBlock => {}
                                    _ => break Err(error),
                                },
                            }
                            if now.elapsed() > connect_wait_time {
                                return Err(self::ConnectErrors::WaitTimeExceeded);
                            }
                        }
                    } else {
                        TcpStream::connect(&socket_address)
                    } {
                        Ok(stream) => {
                            info!("connected to {:?}", socket_address);
                            break stream;
                        }
                        Err(err) => {
                            info!("Received error: {:?}", err);
                            error = ConnectErrors::ConnectionError(err);
                        }
                    }
                } else {
                    return Err(error);
                }
            }
        };
        Self::start_read_thread(client, config)
    }
    /// This sets up a server waiting for a client to connect to it.
    /// Afterwards it can be used to send and receive commands.
    /// # Example
    /// ```
    /// let config = TcpIpcConfig {
    ///     read_iteration_wait_time_ns: 1_000,
    ///     shutdown_wait_time_in_ns: 1_000_000,
    /// };
    /// let mut server =
    ///     TcpIpc::<ProtocolExample>::server("127.0.0.1:6666", config).expect("connecting failed");
    /// ```
    pub fn server<T: ToSocketAddrs>(
        socket_addresses: T,
        config: TcpIpcConfig,
    ) -> Result<TcpIpc<P>, ConnectErrors> {
        // connect
        let server = {
            let mut error = self::ConnectErrors::SocketListIsEmpty;
            let mut socket_addresses = socket_addresses
                .to_socket_addrs()
                .map_err(ConnectErrors::SocketListParseError)?;
            loop {
                if let Some(socket_address) = socket_addresses.next() {
                    debug!("trying to connect to {:?}", socket_address);
                    let listener =
                        TcpListener::bind(&socket_address).map_err(ConnectErrors::BindError)?;
                    match loop {
                        match listener.accept() {
                            Ok(stream) => break Ok(stream),
                            Err(error) => match error.kind() {
                                std::io::ErrorKind::WouldBlock => continue,
                                _ => break Err(error),
                            },
                        }
                    } {
                        Ok((stream, socket_address)) => {
                            info!("connected to {:?}", socket_address);
                            break stream;
                        }
                        Err(err) => {
                            info!("Received error: {:?}", err);
                            error = ConnectErrors::ConnectionError(err);
                        }
                    }
                } else {
                    return Err(error);
                }
            }
        };
        Self::start_read_thread(server, config)
    }
    fn start_read_thread(
        tcp_stream: TcpStream,
        config: TcpIpcConfig,
    ) -> Result<TcpIpc<P>, ConnectErrors> {
        // set no_delay (as default), adjust buffer sizes
        tcp_stream
            .set_nodelay(true)
            .map_err(self::ConnectErrors::SetNodelayError)?;
        tcp_stream
            .set_send_buffer_size(
                std::mem::size_of::<<P as Protocol>::HeaderAsArray>() / std::mem::size_of::<u8>(),
            )
            .map_err(self::ConnectErrors::SetSendBufferSizeError)?;
        tcp_stream
            .set_recv_buffer_size(
                std::mem::size_of::<<P as Protocol>::HeaderAsArray>() / std::mem::size_of::<u8>(),
            )
            .map_err(self::ConnectErrors::SetReceiveBufferSizeError)?;

        // start read thread
        let mut tcp_stream_read = tcp_stream
            .try_clone()
            .map_err(ConnectErrors::TryCloneError)?;
        let (message_sender, message_receiver) = std::sync::mpsc::channel();
        let (busy_state_sender, busy_state_receiver) = std::sync::mpsc::channel();
        let (busy_state_query_sender, busy_state_query_receiver) = std::sync::mpsc::channel();
        let (busy_state_queried_sender, busy_state_queried_receiver) = std::sync::mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut protocol = ProtocolBuffer::<P>::new();
            let mut incoming_buffer = [0; BUFFER_SIZE];
            info!("Read thread started");
            let mut counter = 0;
            'read_loop: loop {
                if counter == config.check_count {
                    counter = 0;
                    match shutdown_receiver.try_recv() {
                        Ok(()) => break 'read_loop,
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            // nothing to do
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                            break 'read_loop;
                        }
                    }
                    match busy_state_query_receiver.try_recv() {
                        Ok(()) => match busy_state_queried_sender.send(protocol.get_busy_state()) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::SendError(_)) => {
                                debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                break 'read_loop;
                            }
                        },
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            // nothing to do
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                            break 'read_loop;
                        }
                    }

                    loop {
                        match busy_state_receiver.try_recv() {
                            Ok(busy_state) => protocol.update_busy_state(busy_state),
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                break 'read_loop;
                            }
                        }
                    }
                } else {
                    counter += 1;
                }
                match tcp_stream_read.read(&mut incoming_buffer) {
                    Ok(message_length) => {
                        if message_length == 0 {
                            // nothing to do
                        } else {
                            let mut buffer = &incoming_buffer[0..message_length];
                            debug!("New incoming buffer: {:?}", buffer);
                            while let Some((command, message)) = protocol.process_new_buffer(buffer)
                            {
                                buffer = &[];
                                if let Some((command, message)) =
                                    P::message_is_answered_via_immediate_route(
                                        &command,
                                        &message,
                                        &protocol.get_busy_state(),
                                    ) {
                                    if let Some(message) = P::construct_message(command, &message) {
                                        if let Err(err) = tcp_stream_read.write(&message) {
                                            if message_sender
                                                .send(Err(ReadThreadErrorsInternal::WriteError(
                                                    err,
                                                )))
                                                .is_err()
                                            {
                                                info!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                                break 'read_loop; //disconnected
                                            }
                                        }
                                    } else if message_sender
                                        .send(Err(
                                            ReadThreadErrorsInternal::ImmediateMessageConstructError((
                                                command, message,
                                            )),
                                        ))
                                        .is_err()
                                    {
                                        debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                        break 'read_loop; //disconnected
                                    }
                                } else if message_sender.send(Ok((command, message))).is_err() {
                                    debug!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                    break 'read_loop; //disconnected
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::WouldBlock {
                            // nothing to do, this is interpreted as "no message available"
                        } else if message_sender
                            .send(Err(ReadThreadErrorsInternal::ReadError(err)))
                            .is_err()
                        {
                            break 'read_loop; //disconnected
                        }
                    }
                }
                // wait between loops
                if let Some(read_iteration_wait_time) = config.read_iteration_wait_time {
                    std::thread::sleep(read_iteration_wait_time);
                }
            }
            info!("Read thread finished");
        });
        if let Some(after_connect_wait_time) = config.after_connect_wait_time {
            std::thread::sleep(after_connect_wait_time);
        }
        Ok(TcpIpc {
            shutdown_sender,
            busy_state_sender,
            message_receiver,
            stream: tcp_stream,
            shutdown_wait_time: config.shutdown_wait_time,
            busy_state_query_sender,
            busy_state_queried_receiver,
        })
    }

    /// This updates the busy_state.
    /// # Example
    /// ```
    /// client.update_busy_state(BusyStatesExample::Working);
    /// ```
    pub fn update_busy_state(&mut self, new_busy_state: P::BusyStates) -> BusyStateUpdateResult {
        match self.busy_state_sender.send(new_busy_state) {
            Ok(()) => BusyStateUpdateResult::Success,
            Err(_) => BusyStateUpdateResult::Disconnected,
        }
    }
    /// This queries the current busy_state.
    /// # Example
    /// ```
    /// let current_busy_state = client.get_busy_state();
    /// ```
    pub fn get_busy_state(&mut self) -> Result<P::BusyStates, BusyStateQueryResult> {
        match self.busy_state_query_sender.send(()) {
            Ok(()) => match self.busy_state_queried_receiver.recv() {
                Ok(busy_state) => Ok(busy_state),
                Err(std::sync::mpsc::RecvError) => Err(BusyStateQueryResult::Disconnected),
            },
            Err(std::sync::mpsc::SendError(())) => Err(BusyStateQueryResult::Disconnected),
        }
    }
    /// This function check if a message was received and returns it, if so.
    /// If no message is available (or if a message is only partial available and more data is neceesary), Ok(None) is return.
    /// # Example
    /// ```
    /// let message = client.get_message();
    /// ```
    pub fn get_message(&mut self) -> Result<Option<Message<P>>, ReadThreadErrors<P>> {
        match self.message_receiver.try_recv() {
            Ok(Ok(x)) => Ok(Some(x)),
            Ok(Err(x)) => Err(match x {
                ReadThreadErrorsInternal::WriteError(x) => ReadThreadErrors::WriteError(x),
                ReadThreadErrorsInternal::ReadError(x) => ReadThreadErrors::ReadError(x),
                ReadThreadErrorsInternal::ImmediateMessageConstructError(x) => {
                    ReadThreadErrors::ImmediateMessageConstructError(x)
                }
            }),
            Err(TryRecvError::Disconnected) => Err(ReadThreadErrors::Disconnected),
            Err(TryRecvError::Empty) => Ok(None),
        }
    }
    /// This function attemps to clear the message queue.
    /// To do this, it waits a given duration.
    /// Then it calls get_message until no message is received, or an error is received (which is returned in turn).
    /// # Example
    /// ```
    /// let result = client.clear_message_queue(std::time::Duration::from_micros(10_000));
    /// ```
    pub fn clear_message_queue(
        &mut self,
        sleep_time: Option<std::time::Duration>,
    ) -> Result<(), ReadThreadErrors<P>> {
        if let Some(sleep_time) = sleep_time {
            std::thread::sleep(sleep_time);
        }
        loop {
            match self.get_message() {
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(()),
                Err(x) => return Err(x),
            }
        }
    }
    /// This function awaits for a message.
    /// If no message is received during the wait time, Ok(None) is returned.
    /// If some message is received, Ok(Some((command, payload))) is returned.
    /// If an error happens, Err(x) is returned.
    /// # Example
    /// ```
    /// let message = client.await_message(std::time::Duration::from_micros(10_000), std::time::Duration::from_nanos(2_000));
    /// ```
    pub fn await_message(
        &mut self,
        maximal_wait_time: std::time::Duration,
        iteration_wait_time: Option<std::time::Duration>,
    ) -> Result<Option<Message<P>>, ReadThreadErrors<P>> {
        let instant = std::time::Instant::now();
        while instant.elapsed() < maximal_wait_time {
            match self.get_message() {
                Ok(Some(x)) => return Ok(Some(x)),
                Ok(None) => {
                    if let Some(iteration_wait_time) = iteration_wait_time {
                        std::thread::sleep(iteration_wait_time)
                    }
                }
                Err(x) => return Err(x),
            }
        }
        Ok(None)
    }
    /// This function writes/sends a message. The message is given as command (as enum-variant) & a payload/message.
    /// Then the message header is added and send via TCP, including the message.
    /// If an error occurs, Err(x) is returned.
    /// If the message is writen successfully, Ok(()) is returned.
    /// # Example
    /// ```
    /// let message = client.write_message(ProtocolExampleCommands::Start, "ok".as_bytes());
    /// ```
    pub fn write_message(
        &mut self,
        command: P::Commands,
        message_: &[u8],
    ) -> Result<(), WriteMessageErrors> {
        let message = P::construct_message(command, message_)
            .ok_or(WriteMessageErrors::MessageConstructionFailed)?;
        let result = self
            .stream
            .write_all(&message)
            .map_err(WriteMessageErrors::MessageSendFailed);
        info!("Message send succesfully:{:?}", (command, message_));
        result
    }
    /// Attemps to close the TCP-connection
    /// Since the receiving side might not implement any shutdown functionality, this is optionally (and not included in Drop).
    pub fn shutdown(self) -> Result<(), ShutdownError> {
        let shutdown_requested_succesfully = match self.shutdown_sender.send(()) {
            Ok(()) => {
                debug!("Shutdown send successfully.");
                true
            }
            Err(_) => {
                warn!("Send of shutdown failed.");
                false
            }
        };

        if let Some(shutdown_wait_time) = self.shutdown_wait_time {
            std::thread::sleep(shutdown_wait_time);
        }
        let shutdown_succesfully = match self.stream.shutdown(std::net::Shutdown::Both) {
            Ok(()) => {
                debug!("Shutdown successfully.");
                true
            }
            Err(_) => {
                warn!("Shutdown failed.");
                false
            }
        };
        if !shutdown_requested_succesfully || !shutdown_succesfully {
            Err(ShutdownError {
                shutdown_succesfully,
                shutdown_requested_succesfully,
            })
        } else {
            Ok(())
        }
    }
    /// Attemps to change the Tcp-Stream "NoDelay"-Option
    pub fn set_nodelay(&mut self, no_delay: bool) -> Result<(), std::io::Error> {
        self.stream.set_nodelay(no_delay)
    }
    /// Attemps to get the Tcp-Stream "NoDelay"-Option
    pub fn get_nodelay(&self) -> Result<bool, std::io::Error> {
        self.stream.nodelay()
    }
}
/// The error type for a shutdown attemp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShutdownError {
    /// Indicates if the request was successfully transmitted.
    pub shutdown_requested_succesfully: bool,
    /// Indicates if the shutdown was successful.
    pub shutdown_succesfully: bool,
}
