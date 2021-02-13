extern crate serde;
extern crate serde_json;

use std::{io, thread};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use mio::{Events, Interest, Poll, Registry, Token};
use mio::event::Event;
use mio::net::{TcpListener, TcpStream};
use serde::{Deserialize, Serialize};

use crate::{Context, Block, p2p::Message, p2p::State, p2p::Peer, p2p::Peers};
use std::net::{SocketAddr, IpAddr, SocketAddrV4};
use std::borrow::BorrowMut;

const SERVER: Token = Token(0);
const POLL_TIMEOUT: Option<Duration> = Some(Duration::from_millis(3000));
pub const LISTEN_PORT: u16 = 4244;

pub struct Network {
    context: Arc<Mutex<Context>>
}

impl Network {
    pub fn new(context: Arc<Mutex<Context>>) -> Self {
        Network { context }
    }

    pub fn start(&mut self) -> Result<(), String> {
        let (listen_addr, peers_addrs) = {
            let c = self.context.lock().unwrap();
            (c.settings.listen.clone(), c.settings.peers.clone())
        };

        // Starting server socket
        let addr = listen_addr.parse().expect("Error parsing listen address");
        let mut server = TcpListener::bind(addr).expect("Can't bind to address");
        println!("Started node listener on {}", server.local_addr().unwrap());

        let mut events = Events::with_capacity(64);
        let mut poll = Poll::new().expect("Unable to create poll");
        poll.registry().register(&mut server, SERVER, Interest::READABLE).expect("Error registering poll");
        let context = self.context.clone();
        thread::spawn(move || {
            // Give UI some time to appear :)
            thread::sleep(Duration::from_millis(2000));
            // Unique token for each incoming connection.
            let mut unique_token = Token(SERVER.0 + 1);
            // States of peer connections, and some data to send when sockets become writable
            let mut peers = Peers::new();
            // Starting peer connections to bootstrap nodes
            connect_peers(peers_addrs, &mut poll, &mut peers, &mut unique_token);

            loop {
                // Poll Mio for events, blocking until we get an event.
                poll.poll(&mut events, POLL_TIMEOUT).expect("Error polling sockets");

                // Process each event.
                for event in events.iter() {
                    println!("Event for socket {} is {:?}", event.token().0, &event);
                    // We can use the token we previously provided to `register` to determine for which socket the event is.
                    match event.token() {
                        SERVER => {
                            // If this is an event for the server, it means a connection is ready to be accepted.
                            let connection = server.accept();
                            match connection {
                                Ok((mut stream, mut address)) => {
                                    // Checking if it is an ipv4-mapped ipv6 if yes convert to ipv4
                                    if address.is_ipv6() {
                                        if let IpAddr::V6(ipv6) = address.ip() {
                                            if let Some(ipv4) = ipv6.to_ipv4() {
                                                address = SocketAddr::V4(SocketAddrV4::new(ipv4, address.port()))
                                            }
                                        }
                                    }

                                    println!("Accepted connection from: {}", address);
                                    let token = next(&mut unique_token);
                                    poll.registry().register(&mut stream, token, Interest::READABLE).expect("Error registering poll");
                                    peers.add_peer(token, Peer::new(address, stream, State::Connected, true));
                                }
                                Err(_) => {}
                            }
                        }
                        token => {
                            match peers.get_mut_peer(&token) {
                                Some(peer) => {
                                    match handle_connection_event(context.clone(), &mut peers, &poll.registry(), &event) {
                                        Ok(result) => {
                                            if !result {
                                                peers.remove_peer(&token);
                                            }
                                        }
                                        Err(_err) => {
                                            peers.remove_peer(&token);
                                        }
                                    }
                                }
                                None => { println!("Odd event from poll"); }
                            }
                        }
                    }
                }

                // Send pings to idle peers
                let height = { context.lock().unwrap().blockchain.height() };
                peers.send_pings(poll.registry(), height);
                peers.connect_new_peers(poll.registry(), &mut unique_token);
            }
        });
        Ok(())
    }
}

fn handle_connection_event(context: Arc<Mutex<Context>>, peers: &mut Peers, registry: &Registry, event: &Event) -> io::Result<bool> {
    if event.is_error() || (event.is_read_closed() && event.is_write_closed()) {
        return Ok(false);
    }

    if event.is_readable() {
        let data = {
            let mut peer = peers.get_mut_peer(&event.token()).expect("Error getting peer for connection");
            let mut stream = peer.get_stream();
            read_message(&mut stream)
        };

        if data.is_ok() {
            let data = data.unwrap();
            match Message::from_bytes(data) {
                Ok(message) => {
                    println!("Got message from socket {}: {:?}", &event.token().0, &message);
                    let new_state = handle_message(context.clone(), message, peers, &event.token());
                    let mut peer = peers.get_mut_peer(&event.token()).unwrap();
                    let mut stream = peer.get_stream();
                    match new_state {
                        State::Message { data } => {
                            if event.is_writable() {
                                // TODO handle all errors and buffer data to send
                                send_message(stream, &data);
                            } else {
                                registry.reregister(stream, event.token(), Interest::WRITABLE).unwrap();
                                peer.set_state(State::Message { data });
                            }
                        }
                        State::Connecting => {}
                        State::Connected => {}
                        State::Idle { .. } => {
                            peer.set_state(State::idle());
                        }
                        State::Error => {}
                        State::Banned => {}
                        State::Offline { .. } => {
                            peer.set_state(State::offline(1));
                        }
                    }
                }
                Err(_) => { return Ok(false); }
            }
        } else {
            // Consider connection as unreliable
            return Ok(false);
        }
    }

    if event.is_writable() {
        //println!("Socket {} is writable", event.token().0);
        match peers.get_mut_peer(&event.token()) {
            None => {}
            Some(peer) => {
                match peer.get_state().clone() {
                    State::Connecting => {
                        println!("Sending hello to socket {}", event.token().0);
                        let data: String = {
                            let mut c = context.lock().unwrap();
                            let message = Message::hand(&c.settings.origin, c.settings.version, c.settings.public);
                            serde_json::to_string(&message).unwrap()
                        };
                        send_message(peer.get_stream(), &data.into_bytes());
                        //println!("Sent hello through socket {}", event.token().0);
                    }
                    State::Message { data } => {
                        println!("Sending data to socket {}: {}", event.token().0, &String::from_utf8(data.clone()).unwrap());
                        send_message(peer.get_stream(), &data);
                    }
                    State::Connected => {}
                    State::Idle { from } => {
                        println!("Odd version of pings :)");
                        if from.elapsed().as_secs() >= 30 {
                            let data: String = {
                                let mut c = context.lock().unwrap();
                                let message = Message::ping(c.blockchain.height());
                                serde_json::to_string(&message).unwrap()
                            };
                            send_message(peer.get_stream(), &data.into_bytes());
                        }
                    }
                    State::Error => {}
                    State::Banned => {}
                    State::Offline { .. } => {}
                }
                registry.reregister(peer.get_stream(), event.token(), Interest::READABLE).unwrap();
            }
        }
    }

    Ok(true)
}

fn read_message(stream: &mut &mut TcpStream) -> Result<Vec<u8>, Vec<u8>> {
    let data_size = match stream.read_u32::<BigEndian>() {
        Ok(size) => { size as usize }
        Err(e) => {
            println!("Error reading from socket! {}", e);
            0
        }
    };
    println!("Payload size is {}", data_size);

    // TODO check for very big buffer, make it no more 10Mb
    let mut buf = vec![0u8; data_size];
    let mut bytes_read = 0;
    loop {
        match stream.read(&mut buf[bytes_read..]) {
            Ok(bytes) => {
                bytes_read += bytes;
            }
            // Would block "errors" are the OS's way of saying that the connection is not actually ready to perform this I/O operation.
            Err(ref err) if would_block(err) => break,
            Err(ref err) if interrupted(err) => continue,
            // Other errors we'll consider fatal.
            Err(err) => return Err(buf),
        }
    }
    if buf.len() == data_size {
        Ok(buf)
    } else {
        Err(buf)
    }
}

fn send_message(connection: &mut TcpStream, data: &Vec<u8>) {
    // TODO handle errors
    connection.write_u32::<BigEndian>(data.len() as u32);
    connection.write_all(&data).expect("Error writing to socket");
    connection.flush();
}

fn handle_message(context: Arc<Mutex<Context>>, message: Message, peers: &mut Peers, token: &Token) -> State {
    let my_height = {
        let context = context.lock().unwrap();
        context.blockchain.height()
    };
    match message {
        Message::Hand { origin: origin, version, public } => {
            let context = context.lock().unwrap();
            if origin == context.settings.origin && version == context.settings.version {
                let mut peer = peers.get_mut_peer(token).unwrap();
                peer.set_public(public);
                State::message(Message::shake(&context.settings.origin, context.settings.version, true, context.blockchain.height()))
            } else {
                State::Error
            }
        }
        Message::Shake { origin, version, ok, height } => {
            // TODO check origin and version for compatibility
            if ok {
                if height > my_height {
                    State::message(Message::GetBlock { index: my_height + 1u64 })
                } else {
                    State::message(Message::GetPeers)
                }
            } else {
                State::Error
            }
        }
        Message::Error => { State::Error }
        Message::Ping { height } => {
            if height > my_height {
                State::message(Message::GetBlock { index: my_height + 1u64 })
            } else {
                State::message(Message::pong(my_height))
            }
        }
        Message::Pong { height } => {
            if height > my_height {
                State::message(Message::GetBlock { index: my_height + 1u64 })
            } else {
                State::idle()
            }
        }
        Message::GetPeers => {
            let peer = peers.get_peer(token).unwrap();
            State::message(Message::Peers { peers: peers.get_peers_for_exchange(&peer.get_addr()) })
        }
        Message::Peers { peers: new_peers } => {
            peers.add_peers_from_exchange(new_peers);
            State::idle()
        }
        Message::GetBlock { index } => {
            let context = context.lock().unwrap();
            match context.blockchain.get_block(index) {
                Some(block) => State::message(Message::block(block.index, serde_json::to_string(&block).unwrap())),
                None => State::Error
            }
        }
        Message::Block { index, block } => {
            let block: Block = match serde_json::from_str(&block) {
                Ok(block) => block,
                Err(_) => return State::Error
            };
            // TODO check if the block is good
            let context = context.clone();
            thread::spawn(move || {
                let mut context = context.lock().unwrap();
                context.blockchain.add_block(block);
                context.bus.post(crate::event::Event::BlockchainChanged)
            });
            State::idle()
        }
    }
}

/// Connecting to configured (bootstrap) peers
fn connect_peers(peers_addrs: Vec<String>, poll: &mut Poll, peers: &mut Peers, unique_token: &mut Token) {
    for peer in peers_addrs.iter() {
        let addr: SocketAddr = peer.parse().expect(&format!("Error parsing peer address {}", &peer));
        match TcpStream::connect(addr.clone()) {
            Ok(mut stream) => {
                println!("Created connection to peer {}", &peer);
                let token = next(unique_token);
                poll.registry().register(&mut stream, token, Interest::WRITABLE).unwrap();
                let mut peer = Peer::new(addr, stream, State::Connecting, false);
                peer.set_public(true);
                peers.add_peer(token, peer);
            }
            Err(e) => {
                println!("Error connecting to peer {}: {}", &peer, e);
            }
        }
    }
}

pub(crate) fn next(current: &mut Token) -> Token {
    let next = current.0;
    current.0 += 1;
    Token(next)
}

fn would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

fn interrupted(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}
