use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};

/*use num_derive::*;

#[macro_use]
extern crate enum_primitive_derive;
extern crate num_traits;

use num_traits::{FromPrimitive, ToPrimitive};*/

//mod protocol;

//use num_traits::FromPrimitive;
use simplelog::*;

mod protocol;
use self::protocol::*;

struct Config {
    connect_wait_time_ms: u64,
}

#[derive(Debug)]
struct Client<P: Protocol> {
    protocol: P,
    config: Config,
}
impl<P: Protocol> Client<P> {
    fn connect<T: ToSocketAddrs>(
        socket_addresses: T,
        config: Config,
    ) -> Result<Self, std::io::Error> {
        let mut socket_addresses = socket_addresses.to_socket_addrs()?;
        let mut error =
            std::io::Error::new(std::io::ErrorKind::Other, "Socket Address list is empty");
        // connect
        let mut stream = loop {
            if let Some(socket_address) = socket_addresses.next() {
                info!("try to connect to {:?}", socket_address);
                match TcpStream::connect_timeout(&socket_address, timeout_time) {
                    Ok(stream) => {
                        info!("connected");
                        break stream;
                    }
                    Err(err) => {
                        info!("Received error: {:?}", err);
                        error = err;
                    }
                }
            } else {
                return Err(error);
            }
        };
    }
}

struct ProtocolBuffer<P: Protocol> {
    current_command: Option<P::Commands>,
    current_target: usize,
    current_message: Vec<u8>,
    incoming_buffer_vec: Vec<u8>,
    busy_state: P::BusyStates,
}
impl<P: Protocol> ProtocolBuffer<P> {
    fn new() -> Self {
        Self {
            current_command: None,
            current_target: 0,
            current_message: Vec::new(),
            incoming_buffer_vec: Vec::new(),
            busy_state: P::idle(),
        }
    }
    fn process_new_buffer(&mut self, incoming_buffer: &[u8]) -> Option<(P::Commands, Vec<u8>)> {
        self.incoming_buffer_vec.extend_from_slice(incoming_buffer);
        if let Some(command) = self.current_command {
            if self.incoming_buffer_vec.len() + self.current_message.len() < self.current_target {
                self.current_message.append(&mut self.incoming_buffer_vec);
                None
            } else {
                let mut completed_message = self.current_message.split_off(0);
                let mut to_append = self
                    .incoming_buffer_vec
                    .split_off(self.current_target - completed_message.len());
                completed_message.append(&mut self.incoming_buffer_vec);
                self.incoming_buffer_vec.append(&mut to_append);
                self.current_target = 0; //not strictly necessary
                self.current_command = None;
                Some((command, completed_message))
            }
        } else {
            if let Some((header, message)) =
                P::message_slice_to_header_array(self.incoming_buffer_vec.as_slice())
            {
                let (command, length) = match P::parse_header(header) {
                    Ok((command, length)) => (command, length),
                    Err((err, message)) => {
                        panic!("parse error: {:?}, incoming header: {:?}", err, message)
                    }
                };
                self.current_command = Some(command);
                self.current_target = length;
                self.current_message = message.to_vec(); // capicity can also be set already
                self.incoming_buffer_vec = self.current_message.split_off(length);
                self.current_message
                    .reserve(length - self.current_message.len());
                self.process_new_buffer(&[]) // process remaining buffer
            } else {
                None
            }
        }
    }
}

fn main() {
    start_server();
    // start client thread
    let mut client = TcpStream::connect_timeout(
        &"127.0.0.1:6666".to_owned().into(),
        std::time::Duration::from_millis(5_000),
    )
    .expect("client unwrap");
    client
        .set_nonblocking(true)
        .expect("set_nonblocking call failed");
    let mut protocol = ProtocolBuffer::<ProtocolExample>::new();

    std::thread::sleep(std::time::Duration::from_micros(10));

    let (message_sender, message_receiver) = std::sync::mpsc::channel();
    let (busy_state_sender, busy_state_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut incoming_buffer = [0; 128];
        loop {
            loop {
                match busy_state_receiver.try_recv() {
                    Ok(busy_state) => protocol.busy_state = busy_state,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("disconnected!"),
                }
            }
            match client.read(&mut incoming_buffer) {
                Ok(message_length) => {
                    if message_length == 0 {
                        // nothing to do
                    } else {
                        let mut buffer = &incoming_buffer[0..message_length];
                        while let Some((command, message)) = protocol.process_new_buffer(buffer) {
                            buffer = &[];
                            if let Some((command, message)) =
                                ProtocolExample::message_is_send_via_immediate_route(
                                    &command,
                                    &message,
                                    &protocol.busy_state,
                                ) {
                                let message =
                                    ProtocolExample::construct_message(command, &message).unwrap();
                                client.write(&message).unwrap();
                            } else {
                                message_sender.send((command, message)).unwrap();
                            }
                        }
                    }
                }
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                    } else {
                        panic!("read error{:?}", err)
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_micros(10)); // wait between loops
        }
    });

    busy_state_sender.send(BusyStatesExample::Working).unwrap();
    busy_state_sender.send(BusyStatesExample::Idle).unwrap();
    for _ in 0..3 {
        let (c, m) = message_receiver.recv().unwrap();
        println!("{:?}", (c, m));
    }
    std::thread::sleep(std::time::Duration::from_micros(500_000));
}

fn start_server() {
    // start server thread
    std::thread::spawn(move || {
        let (mut server, socket_address) = TcpListener::bind("127.0.0.1:6666")
            .unwrap()
            .accept()
            .unwrap();
        println!("server connected to {:?}", socket_address);

        use self::CommandsExample::*;

        println!("---------");
        let message = ProtocolExample::construct_message(Start, &[b'a']).unwrap();
        server.write(&message).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        println!("---------");
        let message = ProtocolExample::construct_message(Funny, &[b'a', 0, 1, 2, 3, 4]).unwrap();
        server.write(&message).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        println!("---------");
        let message = ProtocolExample::construct_message(Start, &[b'b', 0, 1, 4]).unwrap();
        server.write(&message).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    std::thread::sleep(std::time::Duration::from_micros(10));
}

const BUFFER_SIZE: usize = 512;
use std::fmt::Debug;
/// This trait represents a message protocol.
/// The associated type "Commands" represents the possible actions like Wait, Start, Stop,  etc.
/// The associated type "State" is used to respond immediately to some commands. For example, during a long computation the client still can answer immediately, e.g. if the server queries the clients current state.
pub trait ProtocolTrait {
    type Commands: Debug + Send + Clone + Copy + 'static;
    type States: Debug + Send + Clone + 'static;
    const HEADER_SIZE: usize;
    fn new() -> Self;
    /// Define an default state which is used to initialize the client
    fn get_default_state() -> Self::States;
    /// Checks if a received command requires an immediate action and - if so - return the message which will be send to the server.
    fn immediate_response_is_necessary(
        command: Self::Commands,
        current_state: Self::States,
    ) -> Option<Vec<u8>>;
    /// Parses an received bit stream into (possibly several) messages and appends those to a given set of messages
    /// This has a default implementation
    fn parse_message(
        &mut self,
        unparsed_messages: &mut Vec<u8>,
        parsed_messages: &mut Vec<(Self::Commands, Vec<u8>)>,
    ) {
        if let Some((size, command)) = self.get_header() {
            if unparsed_messages.len() < size {
                // nothing to do
            } else {
                // parse message
                let new_message = (command, unparsed_messages[0..size].to_vec());
                info!("message parsed: {:?}", new_message);
                parsed_messages.push(new_message);
                self.update_header(None);
                for _ in 0..size {
                    unparsed_messages.remove(0);
                }
                // recursively call again to parse remaining messages
                self.parse_message(unparsed_messages, parsed_messages)
            }
        } else if unparsed_messages.len() < Self::HEADER_SIZE {
            // nothing to do
        } else {
            // parse header
            self.update_header(Some(&unparsed_messages[0..Self::HEADER_SIZE]));
            for _ in 0..Self::HEADER_SIZE {
                unparsed_messages.remove(0);
            }
            // recursively call again to parse remaining messages
            self.parse_message(unparsed_messages, parsed_messages)
        }
    }
    fn update_header(&mut self, header: Option<&[u8]>);
    fn get_header(&self) -> Option<(usize, Self::Commands)>;
}

#[derive(Debug, Clone, Copy, Primitive)]
enum ProtocolExampleCommands {
    Unknown = 0,
    Start = 1,
    Stop = 2,
    Pause = 3,
    Continue = 4,
    Error = 999,
    QueryIsBusy = 12,
}
#[derive(Debug, Clone)]
enum ProtocolExampleStates {
    WaitingForRequest,
    Working(Vec<u8>),
}

#[derive(Debug)]
struct ProtocolExample {
    header: Option<(usize, ProtocolExampleCommands)>,
}

impl ProtocolTrait for ProtocolExample {
    type Commands = ProtocolExampleCommands;
    type States = ProtocolExampleStates;
    const HEADER_SIZE: usize = 6;
    fn new() -> Self {
        ProtocolExample { header: None }
    }
    fn get_default_state() -> Self::States {
        ProtocolExampleStates::WaitingForRequest
    }
    fn immediate_response_is_necessary(
        command: Self::Commands,
        current_state: Self::States,
    ) -> Option<Vec<u8>> {
        use self::ProtocolExampleCommands::*;
        use self::ProtocolExampleStates::*;
        match command {
            Unknown => None,
            Start => None,
            Stop => None,
            Pause => None,
            Continue => None,
            Error => None,
            QueryIsBusy => match current_state {
                WaitingForRequest => None,
                Working(message) => Some(message),
            },
        }
    }
    fn update_header(&mut self, header: Option<&[u8]>) {
        if let Some(h) = header {
            const SIZE_BYTES: usize = 3;
            assert!(SIZE_BYTES < Self::HEADER_SIZE);
            let mut payload_size = 0usize;
            for i in 0..SIZE_BYTES {
                payload_size +=
                    usize::from(h[i]) * 256u32.pow((SIZE_BYTES - 1 - i) as u32) as usize;
            }
            let mut command = 0u32;
            for i in 0..(Self::HEADER_SIZE - SIZE_BYTES) {
                command += u32::from(h[i + SIZE_BYTES])
                    * 256u32.pow((Self::HEADER_SIZE - SIZE_BYTES - 1 - i) as u32);
            }
            let command = if let Some(x) = ProtocolExampleCommands::from_u32(command) {
                x
            } else {
                debug!(
                    "received unknown header {:?}, parsed command number{:?}",
                    header, command
                );
                ProtocolExampleCommands::Error
            };

            self.header = Some((payload_size, command));
        } else {
            self.header = None;
        }
    }
    fn get_header(&self) -> Option<(usize, Self::Commands)> {
        self.header
    }
}

#[derive(Debug)]
struct Client<Protocol: ProtocolTrait> {
    incoming_message_queue: std::sync::Arc<
        std::sync::Mutex<(
            Vec<(<Protocol as ProtocolTrait>::Commands, Vec<u8>)>,
            <Protocol as ProtocolTrait>::States,
        )>,
    >,
}

impl<Protocol: ProtocolTrait> Client<Protocol> {
    fn new() -> Self {
        Client {
            incoming_message_queue: std::sync::Arc::new(std::sync::Mutex::new((
                Vec::new(),
                <Protocol as ProtocolTrait>::get_default_state(),
            ))),
        }
    }
    fn connect<T: ToSocketAddrs>(
        &mut self,
        socket_addresses: T,
        timeout_time: std::time::Duration,
    ) -> Result<()> {
        let mut socket_addresses = socket_addresses.to_socket_addrs()?;
        let mut error =
            std::io::Error::new(std::io::ErrorKind::Other, "Socket Address list is empty");
        // connect
        let mut stream = loop {
            if let Some(socket_address) = socket_addresses.next() {
                info!("try to connect to {:?}", socket_address);
                match TcpStream::connect_timeout(&socket_address, timeout_time) {
                    Ok(stream) => {
                        info!("connected");
                        break stream;
                    }
                    Err(err) => {
                        info!("Received error: {:?}", err);
                        error = err;
                    }
                }
            } else {
                return Err(error);
            }
        };
        // start read thread
        let messages_inside = self.incoming_message_queue.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; BUFFER_SIZE as usize];
            let mut unparsed_messages = Vec::with_capacity(BUFFER_SIZE);
            let mut protocol = Protocol::new();
            loop {
                match stream.read(&mut buffer) {
                    Ok(n) => {
                        if n == 0 {
                            info!("package of size 0 received, so shuting down");
                            break;
                        } else {
                            unparsed_messages.extend_from_slice(&buffer[0..n]);
                            let mut parsed_messages = Vec::new();
                            Protocol::parse_message(
                                &mut protocol,
                                &mut unparsed_messages,
                                &mut parsed_messages,
                            );
                            if !parsed_messages.is_empty() {
                                if let Ok(mut inner_data) = messages_inside.lock() {
                                    for new_message in parsed_messages {
                                        if let Some(message) = <Protocol as ProtocolTrait>::immediate_response_is_necessary(
                                            new_message.0, inner_data.1.clone())
                                        {
                                            info!("immediate response necessary, answering: {:?}", message);
                                            stream.write(message.as_slice()).unwrap();

                                        } else {
                                        inner_data.0.push(new_message);
                                    }
                                    }
                                } else {
                                    error!("Failed to lock mutex in read thread");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => error!("error during read: {:?}", error),
                }
            }
        });

        Ok(())
    }
}
/*fn main() {
    TermLogger::init(LevelFilter::Info, Config::default()).unwrap();
    let mut client = Client::<ProtocolExample>::new();
    client
        .connect("127.0.0.1:8080", std::time::Duration::from_millis(200))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(202));
    {
        let mut messages = client.incoming_message_queue.lock().unwrap();
        while let Some(message) = messages.0.pop() {
            println!("{:?}", message);
        }
    }
    println!("--------");
    std::thread::sleep(std::time::Duration::from_millis(200));
    {
        let messages = client.incoming_message_queue.lock().unwrap();
        for message in messages.0.iter() {
            println!("{:?}", message);
        }
    }
}
*/
