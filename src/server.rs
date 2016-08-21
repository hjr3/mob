use std::io;
use std::rc::Rc;

use mio::*;
use mio::tcp::*;
use slab;

use connection::Connection;

type Slab<T> = slab::Slab<T, Token>;

pub struct Server {
    // main socket for our server
    sock: TcpListener,

    // token of our server. we keep track of it here instead of doing `const SERVER = Token(0)`.
    token: Token,

    // a list of connections _accepted_ by our server
    conns: Slab<Connection>,

    // a list of events to process
    events: Events,
}

impl Server {
    pub fn new(sock: TcpListener) -> Server {
        Server {
            sock: sock,

            // I don't use Token(0) because kqueue will send stuff to Token(0)
            // by default causing really strange behavior. This way, if I see
            // something as Token(0), I know there are kqueue shenanigans
            // going on.
            token: Token(1),

            // SERVER is Token(1), so start after that
            // we can deal with a max of 126 connections
            conns: Slab::new_starting_at(Token(2), 128),

            // list of events from the poller that the server needs to process
            events: Events::new(),
        }
    }

    pub fn run(&mut self, poll: &mut Poll) -> io::Result<()> {

        try!(self.register(poll));

        info!("Server run loop starting...");
        loop {
            let cnt = try!(poll.poll(&mut self.events, None));

            let mut i = 0;

            trace!("processing events... cnt={}; len={}", cnt, self.events.len());

            // Iterate over the notifications. Each event provides the token
            // it was registered with (which usually represents, at least, the
            // handle that the event is about) as well as information about
            // what kind of event occurred (readable, writable, signal, etc.)
            while i < cnt {
                // TODO this would be nice if it would turn a Result type. trying to convert this
                // into a io::Result runs into a problem because .ok_or() expects std::Result and
                // not io::Result
                let event = self.events.get(i).expect("Failed to get event");

                trace!("event={:?}; idx={:?}", event, i);
                self.ready(poll, event.token(), event.kind());

                i += 1;
            }

            self.tick(poll);
        }
    }

    /// Register Server with the poller.
    ///
    /// This keeps the registration details neatly tucked away inside of our implementation.
    pub fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        poll.register(
            &self.sock,
            self.token,
            EventSet::readable(),
            PollOpt::edge()
        ).or_else(|e| {
            error!("Failed to register server {:?}, {:?}", self.token, e);
            Err(e)
        })
    }

    fn tick(&mut self, poll: &mut Poll) {
        trace!("Handling end of tick");

        let mut reset_tokens = Vec::new();

        for c in self.conns.iter_mut() {
            if c.is_reset() {
                reset_tokens.push(c.token);
            } else if c.is_idle() {
                c.reregister(poll)
                    .unwrap_or_else(|e| {
                        warn!("Reregister failed {:?}", e);
                        c.mark_reset();
                        reset_tokens.push(c.token);
                    });
            }
        }

        for token in reset_tokens {
            match self.conns.remove(token) {
                Some(_c) => {
                    debug!("reset connection; token={:?}", token);
                }
                None => {
                    warn!("Unable to remove connection for {:?}", token);
                }
            }
        }
    }

    fn ready(&mut self, poll: &mut Poll, token: Token, event: EventSet) {
        debug!("{:?} event = {:?}", token, event);
        assert!(token != Token(0), "[BUG]: Received event for Server token {:?}", token);

        if event.is_error() {
            warn!("Error event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        if event.is_hup() {
            trace!("Hup event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        // We never expect a write event for our `Server` token . A write event for any other token
        // should be handed off to that connection.
        if event.is_writable() {
            trace!("Write event for {:?}", token);
            assert!(self.token != token, "Received writable event for Server");

            let conn = self.find_connection_by_token(token);

            if conn.is_reset() {
                info!("{:?} has already been reset", token);
                return;
            }

            conn.writable()
                .unwrap_or_else(|e| {
                    warn!("Write event failed for {:?}, {:?}", token, e);
                    conn.mark_reset();
                });
        }

        // A read event for our `Server` token means we are establishing a new connection. A read
        // event for any other token should be handed off to that connection.
        if event.is_readable() {
            trace!("Read event for {:?}", token);
            if self.token == token {
                self.accept(poll);
            } else {

                if self.find_connection_by_token(token).is_reset() {
                    info!("{:?} has already been reset", token);
                    return;
                }

                self.readable(token)
                    .unwrap_or_else(|e| {
                        warn!("Read event failed for {:?}: {:?}", token, e);
                        self.find_connection_by_token(token).mark_reset();
                    });
            }
        }

        if self.token != token {
            self.find_connection_by_token(token).mark_idle();
        }
    }

    /// Accept a _new_ client connection.
    ///
    /// The server will keep track of the new connection and forward any events from the poller
    /// to this connection.
    fn accept(&mut self, poll: &mut Poll) {
        debug!("server accepting new socket");

        loop {
            // Log an error if there is no socket, but otherwise move on so we do not tear down the
            // entire server.
            let sock = match self.sock.accept() {
                Ok(s) => {
                    match s {
                        Some((sock, _)) => sock,
                        None => {
                            debug!("accept encountered WouldBlock");
                            return;
                        }
                    }
                },
                Err(e) => {
                    error!("Failed to accept new socket, {:?}", e);
                    return;
                }
            };

            match self.conns.insert_with(|token| {
                debug!("registering {:?} with poller", token);
                Connection::new(sock, token)
            }) {
                Some(token) => {
                    // If we successfully insert, then register our connection.
                    match self.find_connection_by_token(token).register(poll) {
                        Ok(_) => {},
                        Err(e) => {
                            error!("Failed to register {:?} connection with poller, {:?}", token, e);
                            self.conns.remove(token);
                        }
                    }
                },
                None => {
                    // If we fail to insert, `conn` will go out of scope and be dropped.
                    error!("Failed to insert connection into slab");
                }
            };
        }
    }

    /// Forward a readable event to an established connection.
    ///
    /// Connections are identified by the token provided to us from the poller. Once a read has
    /// finished, push the receive buffer into the all the existing connections so we can
    /// broadcast.
    fn readable(&mut self, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);

        while let Some(message) = try!(self.find_connection_by_token(token).readable()) {

            let rc_message = Rc::new(message);
            // Queue up a write for all connected clients.
            for c in self.conns.iter_mut() {
                c.send_message(rc_message.clone())
                    .unwrap_or_else(|e| {
                        error!("Failed to queue message for {:?}: {:?}", c.token, e);
                        c.mark_reset();
                    });
            }
        }

        Ok(())
    }

    /// Find a connection in the slab using the given token.
    fn find_connection_by_token<'a>(&'a mut self, token: Token) -> &'a mut Connection {
        &mut self.conns[token]
    }
}
