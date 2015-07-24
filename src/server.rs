use std::io;

use mio::*;
use mio::tcp::*;
use mio::util::Slab;

use connection::Connection;

pub struct Server {
    // main socket for our server
    sock: TcpListener,

    // token of our server. we keep track of it here instead of doing `const SERVER = Token(0)`.
    token: Token,

    // a list of connections _accepted_ by our server
    conns: Slab<Connection>,
}

impl Handler for Server {
    type Timeout = ();
    type Message = ();

    fn tick(&mut self, event_loop: &mut EventLoop<Server>) {
        trace!("Handling end of tick");

        let mut reset_tokens = Vec::new();

        for c in self.conns.iter_mut() {
            if c.is_reset() {
                reset_tokens.push(c.token);
            } else {
                c.reregister(event_loop)
                    .unwrap_or_else(|e| {
                        warn!("Reregister failed {:?}", e);
                        c.mark_reset();
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

    fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
        debug!("{:?} events = {:?}", token, events);
        assert!(token != Token(0), "[BUG]: Received event for Server token {:?}", token);

        if events.is_error() {
            warn!("Error event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        if events.is_hup() {
            trace!("Hup event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        // We never expect a write event for our `Server` token . A write event for any other token
        // should be handed off to that connection.
        if events.is_writable() {
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
        if events.is_readable() {
            trace!("Read event for {:?}", token);
            if self.token == token {
                self.accept(event_loop);
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
    }
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
        }
    }

    /// Register Server with the event loop.
    ///
    /// This keeps the registration details neatly tucked away inside of our implementation.
    pub fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.register(
            &self.sock,
            self.token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot()
        ).or_else(|e| {
            error!("Failed to register server {:?}, {:?}", self.token, e);
            Err(e)
        })
    }

    /// Reregister Server with the event loop.
    ///
    /// This keeps the registration details neatly tucked away inside of our implementation.
    fn reregister(&mut self, event_loop: &mut EventLoop<Server>) {
        event_loop.reregister(
            &self.sock,
            self.token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot()
        ).unwrap_or_else(|e| {
            error!("Failed to reregister server {:?}, {:?}", self.token, e);
            event_loop.shutdown();
        })
    }

    /// Accept a _new_ client connection.
    ///
    /// The server will keep track of the new connection and forward any events from the event loop
    /// to this connection.
    fn accept(&mut self, event_loop: &mut EventLoop<Server>) {
        debug!("server accepting new socket");

        self.reregister(event_loop);

        // Log an error if there is no socket, but otherwise move on so we do not tear down the
        // entire server.
        let (sock, _) = match self.sock.accept() {
            Ok(s) => {
                match s {
                    Some(sock) => sock,
                    None => {
                        error!("Failed to accept new socket");
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
            debug!("registering {:?} with event loop", token);
            Connection::new(sock, token)
        }) {
            Some(token) => {
                // If we successfully insert, then register our connection.
                match self.find_connection_by_token(token).register(event_loop) {
                    Ok(_) => {},
                    Err(e) => {
                        error!("Failed to register {:?} connection with event loop, {:?}", token, e);
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

    /// Forward a readable event to an established connection.
    ///
    /// Connections are identified by the token provided to us from the event loop. Once a read has
    /// finished, push the receive buffer into the all the existing connections so we can
    /// broadcast.
    fn readable(&mut self, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);

        while let Some(message) = try!(self.find_connection_by_token(token).readable()) {

            // Queue up a write for all connected clients.
            for c in self.conns.iter_mut() {
                // TODO: use references so we don't have to clone
                let conn_send_buf = message.clone();
                c.send_message(conn_send_buf)
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
