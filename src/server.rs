use std::fmt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};

use mio::tcp::TcpListener;
use mio::util::Slab;
use mio::{
    EventLoop,
    Handler,
    Interest,
    Token,
};
use mio::Timeout as TimeoutHandle;
use capnp::{
    MessageReader,
};

use ClientId;
use Result;
use Error;
use ErrorKind;
use ServerId;
use messages;
use messages_capnp::connection_preamble;
use replica::{Replica, Actions, ReplicaTimeout};
use state_machine::StateMachine;
use store::Store;
use connection::{Connection, ConnectionKind};

const LISTENER: Token = Token(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServerTimeout {
    Replica(ReplicaTimeout),
    Reconnect(Token),
}

/// The Raft Distributed Consensus Algorithm requires two RPC calls to be available:
///
///   * `append_entries` which is used as both a heartbeat (with no payload) and the primary
///     interface for requests.
///   * `request_vote` which is used by candidates during campaigns to obtain a vote.
///
/// A `Server` acts as a replicated state machine. The server's role in the cluster depends on it's
/// own status. It will maintain both volatile state (which can be safely lost) and persistent
/// state (which must be carefully stored and kept safe).
///
/// Currently, the `Server` API is not well defined. **We are looking for feedback and suggestions.**
pub struct Server<S, M> where S: Store, M: StateMachine {

    /// Id of this server.
    id: ServerId,

    /// Raft state machine replica.
    replica: Replica<S, M>,

    /// Connection listener.
    listener: TcpListener,

    /// Collection of connections indexed by token.
    connections: Slab<Connection>,

    /// Index of peer id to connection token.
    peer_tokens: HashMap<ServerId, Token>,

    /// Index of client id to connection token.
    client_tokens: HashMap<ClientId, Token>,

    /// Currently registered replica timeouts.
    replica_timeouts: HashMap<ReplicaTimeout, TimeoutHandle>,

    /// Currently registered reconnection timeouts.
    reconnection_timeouts: HashMap<Token, TimeoutHandle>,
}

/// The implementation of the Server.
impl<S, M> Server<S, M> where S: Store, M: StateMachine {

    fn new(id: ServerId,
           addr: SocketAddr,
           peers: HashMap<ServerId, SocketAddr>,
           store: S,
           state_machine: M) -> Result<(Server<S, M>, EventLoop<Server<S, M>>)> {
        assert!(!peers.contains_key(&id), "peer set must not contain the local server");
        let replica = Replica::new(id, peers.keys().cloned().collect(), store, state_machine);
        let mut event_loop = try!(EventLoop::<Server<S, M>>::new());
        let listener = try!(TcpListener::bind(&addr));
        try!(event_loop.register(&listener, LISTENER));

        let mut server = Server {
            id: id,
            replica: replica,
            listener: listener,
            connections: Slab::new_starting_at(Token(1), 129),
            peer_tokens: HashMap::new(),
            client_tokens: HashMap::new(),
            replica_timeouts: HashMap::new(),
            reconnection_timeouts: HashMap::new(),
        };

        for (peer_id, peer_addr) in peers {
            let token: Token = try!(server.connections
                                          .insert(try!(Connection::peer(peer_id, peer_addr)))
                                          .map_err(|_| Error::Raft(ErrorKind::ConnectionLimitReached)));
            assert!(server.peer_tokens.insert(peer_id, token).is_none());

            let mut connection = &mut server.connections[token];
            connection.set_token(token);
            try!(connection.send_message(&mut event_loop, messages::server_connection_preamble(id)));
        }

        Ok((server, event_loop))
    }

    /// Runs a new Raft server in the current thread.
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the new node.
    /// * `addr` - The address of the new node.
    /// * `peers` - The ID and address of all peers in the Raft cluster.
    /// * `store` - The persistent log store.
    /// * `state_machine` - The client state machine to which client commands will be applied.
    pub fn run(id: ServerId,
               addr: SocketAddr,
               peers: HashMap<ServerId, SocketAddr>,
               store: S,
               state_machine: M) -> Result<()> {
        let (mut server, mut event_loop) = try!(Server::new(id, addr, peers, store, state_machine));
        let actions = server.replica.init();
        server.execute_actions(&mut event_loop, actions);
        event_loop.run(&mut server).map_err(From::from)
    }

    /// Spawns a new Raft server in a background thread.
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the new node.
    /// * `addr` - The address of the new node.
    /// * `peers` - The ID and address of all peers in the Raft cluster.
    /// * `store` - The persistent log store.
    /// * `state_machine` - The client state machine to which client commands will be applied.
    pub fn spawn(id: ServerId,
                 addr: SocketAddr,
                 peers: HashMap<ServerId, SocketAddr>,
                 store: S,
                 state_machine: M) -> Result<JoinHandle<Result<()>>> {
        thread::Builder::new().name(format!("raft::Server({})", id)).spawn(move || {
            Server::run(id, addr, peers, store, state_machine)
        }).map_err(From::from)
    }

    /// Returns the connection to the peer.
    fn peer_connection(&mut self, peer_id: &ServerId) -> &mut Connection {
       let token = self.peer_tokens.get(peer_id).unwrap();
       &mut self.connections[*token]
    }

    /// Finds an existing connection to a client.
    fn client_connection<'a>(&'a mut self, client_id: ClientId) -> Option<&'a mut Connection> {
        match self.client_tokens.get(&client_id) {
            Some(&token) => self.connections.get_mut(token),
            None => None
        }
    }

    fn execute_actions(&mut self,
                       event_loop: &mut EventLoop<Server<S, M>>,
                       actions: Actions) {
        debug!("{:?}: executing actions: {:?}", self, actions);
        let Actions { peer_messages, client_messages, timeouts, clear_timeouts } = actions;

        for (peer, message) in peer_messages {
            let _ = self.peer_connection(&peer)
                        .send_message(event_loop, message);
        }
        for (client, message) in client_messages {
            if let Some(connection) = self.client_connection(client) {
                let _ = connection.send_message(event_loop, message);
            }
        }
        if clear_timeouts {
            for (timeout, &handle) in &self.replica_timeouts {
                assert!(event_loop.clear_timeout(handle),
                        "raft::{:?}: unable to clear timeout: {:?}", self, timeout);
            }
            self.replica_timeouts.clear();
        }
        for timeout in timeouts {
            let duration = timeout.duration_ms();

            // Registering a timeout may only fail if the maximum number of timeouts
            // is already registered, which is by default 65,536. We (should) use a
            // maximum of one timeout per peer, so this unwrap should be safe.
            let handle = event_loop.timeout_ms(ServerTimeout::Replica(timeout), duration)
                                   .unwrap();
            self.replica_timeouts
                .insert(timeout, handle)
                .map(|handle| assert!(event_loop.clear_timeout(handle),
                                      "raft::{:?}: unable to clear timeout: {:?}", self, timeout));
        }
    }

    /// Resets the connection corresponding to the provided token.
    ///
    /// If the connection is to a peer, the server will attempt to reconnect after a waiting
    /// period.
    ///
    /// If the connection is to a client or unknown it will be closed.
    fn reset_connection(&mut self, event_loop: &mut EventLoop<Server<S, M>>, token: Token) {
        let kind = *self.connections[token].kind();
        match kind {
            ConnectionKind::Peer(..) => {
                // Crash if reseting the connection fails.
                let (duration, timeout, handle) = self.connections[token].reset_peer(event_loop).unwrap();

                info!("{:?}: {:?} reset, will attempt to reconnect in {}ms", self,
                      &self.connections[token], duration);
                assert!(self.reconnection_timeouts.insert(token, handle).is_none(),
                        "raft::{:?}: timeout already registered: {:?}", self, timeout);
            },
            ConnectionKind::Client(ref id) => {
                self.connections.remove(token);
                self.client_tokens.remove(id);
            },
            ConnectionKind::Unknown => {
                self.connections.remove(token);
            },
        }
    }
}

impl<S, M> Handler for Server<S, M> where S: Store, M: StateMachine {

    type Message = ();
    type Timeout = ServerTimeout;

    fn ready(&mut self, event_loop: &mut EventLoop<Server<S, M>>, token: Token, events: Interest) {
        trace!("{:?}: ready; token: {:?}; events: {:?}", self, token, events);

        if events.is_error() {
            assert!(token != LISTENER, "raft::{:?}: unexpected error event from LISTENER", self);
            warn!("{:?}: error event on connection {:?}", self, self.connections[token]);
            self.reset_connection(event_loop, token);
            return;
        }

        if events.is_hup() {
            assert!(token != LISTENER, "raft::{:?}: unexpected hup event from LISTENER", self);
            trace!("{:?}: hup event on connection {:?}", self, self.connections[token]);
            self.reset_connection(event_loop, token);
            return;
        }

        if events.is_writable() {
            assert!(token != LISTENER, "raft::{:?}: unexpected writeable event for LISTENER", self);
            if let Err(error) = self.connections[token].writable(event_loop) {
                warn!("{:?}: unable to write to conection {:?}: {}",
                      self, self.connections[token], error);
                self.reset_connection(event_loop, token);
                return;
            }
        }

        if events.is_readable() {
            if token == LISTENER {
                self.listener
                    .accept().map_err(From::from)
                    .and_then(|stream| Connection::unknown(stream.unwrap()))
                    .and_then(|connection| {
                        debug!("{:?}: new connection received: {:?}", self, connection);
                        self.connections
                            .insert(connection)
                            .map_err(|_| Error::Raft(ErrorKind::ConnectionLimitReached))
                    })
                    .and_then(|token| {
                        let mut connection = &mut self.connections[token];
                        connection.set_token(token);
                        connection.register(event_loop)
                    })
                    .unwrap_or_else(|error| warn!("{:?}: unable to accept connection: {}", self, error));
            } else {
                trace!("{:?}: connection readable: {:?}, events: {:?}", self, self.connections[token], events);
                // Read messages from the socket until there are no more.
                while let Some(message) = self.connections[token].readable(event_loop).unwrap() {
                    match *self.connections[token].kind() {
                        ConnectionKind::Peer(id) => {
                            let mut actions = Actions::new();
                            self.replica.apply_peer_message(id, &message, &mut actions);
                            self.execute_actions(event_loop, actions);
                        },
                        ConnectionKind::Client(id) => {
                            let mut actions = Actions::new();
                            self.replica.apply_client_message(id, &message, &mut actions);
                            self.execute_actions(event_loop, actions);
                        },
                        ConnectionKind::Unknown => {
                            let preamble = message.get_root::<connection_preamble::Reader>().unwrap();
                            match preamble.get_id().which().unwrap() {
                                connection_preamble::id::Which::Server(id) => {
                                    let peer_id = ServerId(id);

                                    self.connections[token].set_kind(ConnectionKind::Peer(peer_id));
                                    let prev_token = self.peer_tokens
                                                         .insert(peer_id, token)
                                                         .expect("peer token not found");

                                    // Close the existing connection.
                                    self.connections
                                        .remove(prev_token)
                                        .expect("peer connection not found")
                                        .unregister_peer(event_loop)
                                        .unwrap();

                                    // Clear any timeouts associated with the existing connection.
                                    self.reconnection_timeouts
                                        .remove(&prev_token)
                                        .map(|handle| assert!(event_loop.clear_timeout(handle)));

                                    // TODO: add reconnect messages from replica
                                },
                                connection_preamble::id::Which::Client(Ok(id)) => {
                                    self.connections[token].set_kind(ConnectionKind::Client(ClientId::from_bytes(id).unwrap()));
                                },
                                _ => {
                                    // TODO: reset the connection
                                    unimplemented!()
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Server<S, M>>, timeout: ServerTimeout) {
        trace!("{:?}: timeout: {:?}", self, &timeout);
        match timeout {
            ServerTimeout::Replica(replica) => {
                assert!(self.replica_timeouts.remove(&replica).is_some(),
                        "raft::{:?}: missing timeout: {:?}", self, timeout);
                let mut actions = Actions::new();
                self.replica.apply_timeout(replica, &mut actions);
                self.execute_actions(event_loop, actions);
            },

            ServerTimeout::Reconnect(token) => {
                assert!(self.reconnection_timeouts.remove(&token).is_some(),
                        "raft::{:?}: missing timeout: {:?}", self, timeout);
                self.connections[token]
                    .reconnect_peer(self.id, event_loop)
                    .unwrap_or_else(|error| {
                        warn!("{:?}: unable to reconnect connection {:?}: {}",
                              self, &self.connections[token], error);
                    });
                // TODO: add reconnect messages from replica
            },
        }
    }
}

impl <S, M> fmt::Debug for Server<S, M> where S: Store, M: StateMachine {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Server({})", self.id)
    }
}

#[cfg(test)]
mod test {

    extern crate env_logger;

    use std::collections::HashMap;
    use std::net::{TcpListener, SocketAddr};
    use std::str::FromStr;

    use ServerId;
    use state_machine::NullStateMachine;
    use store::MemStore;
    use super::*;
    use Result;

    use mio::EventLoop;

    type TestServer = Server<MemStore, NullStateMachine>;

    fn new_test_server(peers: HashMap<ServerId, SocketAddr>) -> Result<(TestServer, EventLoop<TestServer>)> {
        Server::new(ServerId::from(0),
                    SocketAddr::from_str("127.0.0.1:0").unwrap(),
                    peers,
                    MemStore::new(),
                    NullStateMachine)
    }

    /// Attempts to grab a local, unbound socket address for testing.
    fn get_unbound_address() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    #[test]
    pub fn test_illegal_peer_address() {
        let _ = env_logger::init();
        let peer_id = ServerId::from(1);
        let mut peers = HashMap::new();
        peers.insert(peer_id, SocketAddr::from_str("127.0.0.1:0").unwrap());
        assert!(new_test_server(peers).is_err());
    }

    #[test]
    pub fn test_unbound_peer_address() {
        let _ = env_logger::init();
        let peer_id = ServerId::from(1);
        let mut peers = HashMap::new();
        peers.insert(peer_id, get_unbound_address());
        let (mut server, mut event_loop) = new_test_server(peers).unwrap();
        event_loop.run_once(&mut server).unwrap();
        // TODO: figure out how to test this
    }
}
