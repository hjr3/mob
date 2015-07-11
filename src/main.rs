extern crate mio;

#[macro_use] extern crate log;
extern crate env_logger;

use std::io;
use std::net::SocketAddr;
use std::str::FromStr;

use mio::*;
use mio::buf::ByteBuf;
use mio::tcp::*;
use mio::util::Slab;

/// A stateful wrapper around a non-blocking stream. This connection is not
/// the SERVER connection. This connection represents the client connections
/// _accepted_ by the SERVER connection.
struct Connection {
    // handle to the accepted socket
    sock: TcpStream,

    // token used to register with the event loop
    token: Token,

    // types of events to listen to. we will usually be in a readable state.
    // when we want to write, make sure we are handling EGAIN properly.
    // when we get EAGAIN do we need to reregister to a writable state. EAGAIN
    // means the kernel's send buffer is full and we need to try to write
    // _again_ during the next even loop tick. by that next tick, the kernel
    // should have drained some of the send buffer.
    // see: http://stackoverflow.com/a/13568962/775246
    interest: EventSet,

    send_queue: Vec<ByteBuf>,
}

impl Connection {
    fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,

            // new connections are only listening for a hang up event when
            // they are first created. we always want to make sure we are 
            // listening for the hang up event. we will additionally listen
            // for readable and writable events later on.
            interest: EventSet::hup(),

            send_queue: Vec::new(),
        }
    }

    // note: if you are wondering why we do not store `event_loop` in
    // Connection, i believe it is because we only want to borrow the loop
    // for a short time and then end the borrow. if we stored it in Connection,
    // then the mio library could not mutate it further.
    //
    // idea: pretty sure we should use a closure here instead, like thread::scoped
    // doing more research into this, mio does not use a closure for perf. i need to find where i
    // read that.
    /// Handle read event from event loop.
    ///
    /// Currently only reads a max of 2048 bytes. Excess bytes are dropped on the floor.
    ///
    /// The recieve buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<ByteBuf> {

        // ByteBuf is a heap allocated slice that mio supports internally. We use this as it does
        // the work of tracking how much of our slice has been used. I chose a capacity of 2048
        // after reading 
        // https://github.com/carllerche/mio/blob/eed4855c627892b88f7ca68d3283cbc708a1c2b3/src/io.rs#L23-27
        // as that seems like a good size of streaming. If you are wondering what the difference
        // between messaged based and continuous streaming read
        // http://stackoverflow.com/questions/3017633/difference-between-message-oriented-protocols-and-stream-oriented-protocols
        // . TLDR: UDP vs TCP. We are using TCP.
        let mut recv_buf = ByteBuf::mut_with_capacity(2048);

        // we are EPOLLET and EPOLLONESHOT, so we _must_ drain the entire
        // socket receive buffer, otherwise the server will hang.
        loop {
            match self.sock.try_read_buf(&mut recv_buf) {
                // the socket receive buffer is empty, so let's move on
                // try_read_buf internally handles EWOULDBLOCK here too
                Ok(None) => {
                    debug!("CONN : we read 0 bytes");
                    break;
                },
                Ok(Some(n)) => {
                    debug!("CONN : we read {} bytes", n);

                    // if we read less than capacity, then we know the
                    // socket is empty and we should stop reading. if we
                    // read to full capacity, we need to keep reading so we
                    // can drain the socket. if the client sent exactly capacity,
                    // we will match the arm above. the recieve buffer will be
                    // full, so extra bytes are being dropped on the floor. to
                    // properly handle this, i would need to push the data into
                    // a growable Vec<u8>.
                    if n < recv_buf.capacity() {
                        break;
                    }
                },
                Err(e) => {
                    error!("Failed to read buffer for token {:?}, error: {}", self.token, e);

                    self.interest.remove(EventSet::readable());
                    break;
                }
            }
        }

        // we are EPOLLET, so the event_loop will deregister our connection before handing off to
        // us. now that we are done, re-register so we can read more later.
        try!(event_loop.reregister(
                &self.sock,
                self.token,
                self.interest,
                PollOpt::edge() | PollOpt::oneshot()
        ));

        Ok(recv_buf.flip())
    }

    /// Handle a writable event from the event loop.
    ///
    /// Send one message from the send queue to the client. If the queue is empty, remove interest
    /// in write events.
    /// TODO: Figure out if sending more than one message is optimal. Maybe we should be trying to
    /// flush until the kernel sends back EAGAIN?
    fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {

        self.send_queue.pop().and_then(|mut buf| {
            match self.sock.try_write_buf(&mut buf) {
                Ok(None) => {
                    debug!("client flushing buf; EWOULDBLOCK");
                },
                Ok(Some(n)) => {
                    debug!("CONN : we wrote {} bytes", n);
                },
                Err(e) => {
                    error!("Failed to send buffer for token {:?}, error: {}", self.token, e);
                }
            };

            Some(())
        });

        if self.send_queue.len() == 0 {
            self.interest.remove(EventSet::writable());
        }

        event_loop.reregister(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the event loop.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    fn queue_message(&mut self, event_loop: &mut EventLoop<Server>, message: ByteBuf) -> io::Result<()> {
        self.send_queue.push(message);
        self.interest.insert(EventSet::writable());

        event_loop.register_opt(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    /// Register interest in read events with the event_loop.
    ///
    /// This will let our connection accept reads starting next event loop tick.
    fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        self.interest.insert(EventSet::readable());

        event_loop.register_opt(
            &self.sock,
            self.token,
            self.interest, 
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    //fn shutdown(&mut self) {
    //    self.sock.shutdown(Shutdown::Both).unwrap();
    //}
}

struct Server {
    // main socket for our server
    sock: TcpListener,

    // token of our server. we keep track of it here instead of doing `const SERVER = Token(0)`.
    token: Token,
    
    // a list of connections _accepted_ by our server
    conns: Slab<Connection>,

}

impl Server {
    fn new(sock: TcpListener) -> Server {
        Server {
            sock: sock,

            // I don't use Token(0) because kqueue will send stuff to Token(0)
            // by default causing really strange behavior. This way, if I see
            // something as Token(0), I know there are kqueue shenanigans
            // going on.
            token: Token(1),

            // SERVER is Token(0), so start after that
            // we can deal with a max of 128 connections
            conns: Slab::new_starting_at(Token(2), 128)
        }
    }

    fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.register_opt(
            &self.sock,
            self.token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    /// Accept a _new_ client connection.
    ///
    /// The server will keep track of the new connection and forward any events from the event loop
    /// to this connection.
    fn accept(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        debug!("server accepting new socket");

        // TODO: figure out what errors we should handle here
        let sock = match self.sock.accept() {
            Ok(s) => {
                match s {
                    Some(sock) => sock,
                    None => {
                        error!("Failed to accept new socket");
                        return Ok(());
                    }
                }
            },
            Err(e) => panic!("{}", e)
        };

        // Slab::insert() returns the index where the connection was inserted. in mio, the Slab is
        // actually defined as `pub type Slab<T> = ::slab::Slab<T, ::Token>;`. now, Token is really
        // just a type wrapper around `usize` and Token implemented `::slab::Index` trait. so,
        // every insert into the connection slab will return the new token we need to register with
        // the event loop. fancy...
        // i use Slab::insert_with() so I don't have to call Slab::insert(), get the token back and
        // _then_ associate it with the connection.
        let _ = self.conns.insert_with(|token| {
            debug!("registering {:?} with event loop", token);
            let mut conn = Connection::new(sock, token);
            conn.register(event_loop).ok().expect("could not register new socket with event loop");
            conn
        }).unwrap();

        // remember we are EPOLLET. this means even our SERVER token needs to reregister.
        event_loop.reregister(
            &self.sock,
            self.token,
            EventSet::readable(),
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    /// Handle a read event from the event loop.
    ///
    /// A read event for our `Server` token means we are establishing a new connection. A read
    /// event for any other token should be handed off to that connection.
    fn readable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            self.accept(event_loop).unwrap();
        } else {

            // if our readable event handler received a token that is _not_ SERVER, then it must be
            // one of the connections in the slab.
            self.conn_readable(event_loop, token).unwrap()
        }
    }

    /// Forward a readable event to an established connection.
    ///
    /// Connections are identified by the token provided to us from the event loop. Once a read has
    /// finished, push the receive buffer into the all the existing connections so we can
    /// broadcast.
    fn conn_readable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);
        let message = try!(self.find_connection_by_token(token).readable(event_loop));

        // notify all connections there is a new message to send
        for conn in self.conns.iter_mut() {
            // TODO: use references so we don't have to clone
            let conn_send_buf = ByteBuf::from_slice(message.bytes());
            try!(conn.queue_message(event_loop, conn_send_buf));
        }

        Ok(())
    }

    /// Handle a write event from the event loop.
    ///
    /// We never expect a write event for our `Server` token . A write event for any other token
    /// should be handed off to that connection.
    fn writable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            panic!("received writable for token 0");
        } else {
            self.find_connection_by_token(token).writable(event_loop).unwrap();
        }
    }

    fn shutdown(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            event_loop.shutdown();
        } else {
            debug!("server conn shutdown; token={:?}", token);

            // TODO: client hup means the socket is already shutdown. Are
            // there cases where the client is still up?
            //self.find_connection_by_token(token).shutdown();

            self.conns.remove(token);
        }
    }

    /// Find a connection in the slab using the given token.
    fn find_connection_by_token<'a>(&'a mut self, token: Token) -> &'a mut Connection {
        &mut self.conns[token]
    }
}

impl Handler for Server {
    type Timeout = ();
    type Message = ();

    fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
        debug!("events = {:?}", events);

        if events.is_error() {
            // TODO: should i do something other than shutdown here?
            self.shutdown(event_loop, token);
            return;
        }

        if events.is_hup() {
            self.shutdown(event_loop, token);
            return;
        }

        if events.is_readable() {
            self.readable(event_loop, token);
        }

        if events.is_writable() {
            self.writable(event_loop, token);
        }
    }
}

fn main() {
    env_logger::init().unwrap();

    let addr: SocketAddr = FromStr::from_str("127.0.0.1:8000").unwrap();
    let sock = TcpListener::bind(&addr).unwrap();

    let mut event_loop = EventLoop::new().unwrap();

    let mut server = Server::new(sock);
    server.register(&mut event_loop).unwrap();

    info!("Even loop starting...");
    event_loop.run(&mut server).unwrap();
}
