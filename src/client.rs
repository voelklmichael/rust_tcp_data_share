pub use super::protocol_buffer::*;

pub use std::io::{Read, Write};
pub use std::net::{TcpListener, TcpStream, ToSocketAddrs};
pub use std::sync::mpsc::TryRecvError;

const BUFFER_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq)]
/// This bundles the time-settings for the protocol
/// # Example
/// ```
/// let config = ClientConfig {
///     connect_wait_time_ms: 5_000,
///     read_iteration_wait_time_ns: 1_000,
///     shutdown_wait_time_in_ns: 1_000_000,
/// };
/// ```
pub struct ClientConfig {
    /// This is the time the server as to accept a TCP-connection.
    pub connect_wait_time_ms: u64,
    /// This is the time the client sleeps between checking for new messages from the server.
    /// Very small values can yield high CPU-usage.
    pub read_iteration_wait_time_ns: u64,
    /// This is the time the client waits for the server to accept a shutdown request.
    pub shutdown_wait_time_in_ns: u64,
}

#[derive(Debug)]
pub enum ReadThreadErrorsInternal<P: Protocol> {
    WriteError(std::io::Error),
    ReadError(std::io::Error),
    ImmediateMessageParseError((P::Commands, Vec<u8>)),
}
#[derive(Debug)]
pub enum ReadThreadErrors<P: Protocol> {
    WriteError(std::io::Error),
    ReadError(std::io::Error),
    ImmediateMessageParseError((P::Commands, Vec<u8>)),
    Disconnected,
}
/// The error type for the connect-function.
#[derive(Debug)]
pub enum ConnectErrors {
    SocketListIsEmpty,
    SocketListParseError(std::io::Error),
    TryCloneError(std::io::Error),
    ConnectionError(std::io::Error),
    SetNonblockingError,
}
/// This is the main type of the library.
/// Here all the logic is bundle.
/// It can be used to easily send and receive messages via TCP, allowing for many different protcols to be used.
pub struct Client<P: Protocol> {
    busy_state_sender: std::sync::mpsc::Sender<P::BusyStates>,
    message_receiver: std::sync::mpsc::Receiver<Result<Message<P>, ReadThreadErrorsInternal<P>>>,
    stream: TcpStream,
    shutdown_sender: std::sync::mpsc::Sender<()>,
    shutdown_wait_time_in_ns: u64,
}

#[derive(Debug, PartialEq, Clone, Copy)]
/// The error type for a BusyState update
pub enum BusyStateUpdateResult {
    /// Update succesful
    Success,
    /// The only posibility for fail is that the connection is already (disgracefully) closed.
    Disconnected,
}
#[derive(Debug)]
pub enum WriteMessageErrors {
    MessageConstructionFailed,
    MessageSendFailed(std::io::Error),
}
impl<P: Protocol> Client<P> {
    /// This connects the client to the server.
    /// # Example
    /// ```
    /// let config = ClientConfig {
    ///     connect_wait_time_ms: 5_000,
    ///     read_iteration_wait_time_ns: 1_000,
    ///     shutdown_wait_time_in_ns: 1_000_000,
    /// };
    /// let mut client =
    ///     Client::<ProtocolExample>::connect("127.0.0.1:6666", config).expect("connecting failed");
    /// ```
    pub fn connect<T: ToSocketAddrs>(
        socket_addresses: T,
        config: ClientConfig,
    ) -> Result<Client<P>, ConnectErrors> {
        // connect
        let client = {
            let mut error = self::ConnectErrors::SocketListIsEmpty;
            let mut socket_addresses = socket_addresses
                .to_socket_addrs()
                .map_err(ConnectErrors::SocketListParseError)?;
            loop {
                if let Some(socket_address) = socket_addresses.next() {
                    debug!("try to connect to {:?}", socket_address);
                    match TcpStream::connect_timeout(
                        &socket_address,
                        std::time::Duration::from_millis(config.connect_wait_time_ms),
                    ) {
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
        // set non-blocking
        if client.set_nonblocking(true).is_err() {
            return Err(self::ConnectErrors::SetNonblockingError);
        }
        // start read thread
        let mut client_read = client.try_clone().map_err(ConnectErrors::TryCloneError)?;
        let (message_sender, message_receiver) = std::sync::mpsc::channel();
        let (busy_state_sender, busy_state_receiver) = std::sync::mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut protocol = ProtocolBuffer::<P>::new();
            let mut incoming_buffer = [0; BUFFER_SIZE];
            info!("Read thread started");
            'read_loop: loop {
                match shutdown_receiver.try_recv() {
                    Ok(()) => break 'read_loop,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // nothing to do
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        info!("Read thread seems to be disconnected from main thread. Will be shut down.");
                        break 'read_loop;
                    }
                }
                loop {
                    match busy_state_receiver.try_recv() {
                        Ok(busy_state) => protocol.update_busy_state(busy_state),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            info!("Read thread seems to be disconnected from main thread. Will be shut down.");
                            break 'read_loop;
                        }
                    }
                }
                match client_read.read(&mut incoming_buffer) {
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
                                        if let Err(err) = client_read.write(&message) {
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
                                            ReadThreadErrorsInternal::ImmediateMessageParseError((
                                                command, message,
                                            )),
                                        ))
                                        .is_err()
                                    {
                                        info!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                        break 'read_loop; //disconnected
                                    }
                                } else if message_sender.send(Ok((command, message))).is_err() {
                                    info!("Read thread seems to be disconnected from main thread. Will be shut down.");
                                    break 'read_loop; //disconnected
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::WouldBlock {
                        } else if message_sender
                            .send(Err(ReadThreadErrorsInternal::ReadError(err)))
                            .is_err()
                        {
                            break 'read_loop; //disconnected
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_nanos(
                    config.read_iteration_wait_time_ns,
                )); // wait between loops
            }
            info!("Read thread finished");
        });
        Ok(Client {
            shutdown_sender,
            busy_state_sender,
            message_receiver,
            stream: client,
            shutdown_wait_time_in_ns: config.shutdown_wait_time_in_ns,
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
                ReadThreadErrorsInternal::ImmediateMessageParseError(x) => {
                    ReadThreadErrors::ImmediateMessageParseError(x)
                }
            }),
            Err(TryRecvError::Disconnected) => Err(ReadThreadErrors::Disconnected),
            Err(TryRecvError::Empty) => Ok(None),
        }
    }
    /// This function writes/sends a message. The message is given as command (as enum-variant) & a payload/message.
    /// Then the message header is added and send via TCP, including the message.
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

        std::thread::sleep(std::time::Duration::from_nanos(
            self.shutdown_wait_time_in_ns,
        ));
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
}
/// The error type for a shutdown attemp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShutdownError {
    /// Indicates if the request was successfully transmitted.
    pub shutdown_requested_succesfully: bool,
    /// Indicates if the shutdown was successful.
    pub shutdown_succesfully: bool,
}