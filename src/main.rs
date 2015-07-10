extern crate mio;

#[macro_use] extern crate log;
extern crate env_logger;

use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;

use mio::*;
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

    // current state of the connection. we will usually be in a readable state.
    // when we want to write, we can write as part of the readable action. only
    // when we get EAGAIN do we need to switch to a writable state. EAGAIN
    // means the kernel's send buffer is full and we need to try to write
    // _again_ during the next even loop tick. by that next tick, the kernel
    // should have drained some of the send buffer.
    // see: http://stackoverflow.com/a/13568962/775246
    interest: EventSet,

    send_queue: Vec<String>,
}

impl Connection {
    fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,

            // new connections are hung up when they are first created
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
    fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<String> {
        // there are more advanced buffers we can create, but to reduce
        // cognitive load while we learn how this all works, use this
        // naive buffer
        let mut buf = [0u8; 1024];
        let mut bytes = 0;

        // we are EPOLLET and EPOLLONESHOT, so we _must_ drain the entire
        // socket receive buffer, otherwise the server will hang.
        loop {
            match self.sock.read(&mut buf) {
                // the socket receive buffer is empty, so let's move on
                Ok(0) => {
                    debug!("CONN : we read 0 bytes");
                    break;
                },
                Ok(n) => {
                    debug!("CONN : we read {} bytes", n);
                    bytes += n;

                    // if we read less than 1024 bytes, then we know the
                    // socket is empty and we should stop reading. if we
                    // read a full 1024, we need to keep reading so we
                    // can drain the socket. if the client sent exactly 1024,
                    // we will match the arm above.
                    // TODO: make 1024 a non-magic number
                    if n < 1024 {
                        break;
                    }
                },
                Err(e) => {
                    debug!("not implemented; client err={:?}", e);

                    // if we remove readable here the only interest left is hup, specified
                    // when we initially created the new connection.
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

        let message = String::from_utf8_lossy(&buf[..bytes]).into_owned();
        debug!("{:?}: tell SERVER to send back: {}", self.token, message);
        Ok(message)
    }

    fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {

        // TODO: handle the case where our connection is shutdown but the event loop still hands us
        // a writable event

        self.send_queue.pop().and_then(|message| {
            write!(self.sock, "message from token {:?}: {}", self.token, message).unwrap_or_else(|e| {
                error!("Failed to send buffer for token {:?}, error: {}", self.token, e);
            });
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

    fn queue_message(&mut self, event_loop: &mut EventLoop<Server>, message: String) -> io::Result<()> {
        self.send_queue.push(message);
        self.interest.insert(EventSet::writable());

        event_loop.register_opt(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    // register the new connection with the event_loop. this will let our connection read
    // next tick.
    fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        self.interest.insert(EventSet::readable());

        event_loop.register_opt(
            &self.sock,
            self.token,
            self.interest, 
            PollOpt::edge() | PollOpt::oneshot()
        )
    }

    fn shutdown(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.deregister(&self.sock)
    }
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

            token: Token(0),

            // SERVER is Token(0), so start after that
            // we can deal with a max of 128 connections
            conns: Slab::new_starting_at(Token(1), 128)
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

    fn accept(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        debug!("server accepting new socket");

        // TODO: figure out what errors we should handle here
        let sock = self.sock.accept().unwrap().unwrap();

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

    // forward a readable event to the connection, identified by the token
    fn conn_readable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);
        let message = try!(self.find_connection_by_token(token).readable(event_loop));

        // notify all connections there is a new message to send
        for conn in self.conns.iter_mut() {
            try!(conn.queue_message(event_loop, message.clone()));
        }

        Ok(())
    }

    // forward a writable event to the connection, identified by the token
    fn conn_writable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) -> io::Result<()> {
        debug!("server conn writable; token={:?}", token);
        self.find_connection_by_token(token).writable(event_loop)
    }

    fn find_connection_by_token<'a>(&'a mut self, token: Token) -> &'a mut Connection {
        &mut self.conns[token]
    }

    fn readable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {

        // normally our hint is ReadHint, but due to the way kqueue works we need to handle HupHint
        // here too. see https://github.com/carllerche/mio/issues/184 for more information.
        //if hint.is_hup() {
        //    self.shutdown(event_loop, token);
        //}

        if self.token == token {
            self.accept(event_loop).unwrap();
        } else {

            // if our readable event handler received a token that is _not_ SERVER, then it must be
            // one of the connections in the slab.
            self.conn_readable(event_loop, token).unwrap()
        }
    }

    fn writable(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            panic!("received writable for token 0");
        } else {
            self.conn_writable(event_loop, token).unwrap()
        }
    }

    fn shutdown(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
        if self.token == token {
            event_loop.shutdown();
        } else {
            debug!("server conn shutdown; token={:?}", token);
            self.find_connection_by_token(token).shutdown(event_loop).unwrap();
        }
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
        }

        if events.is_hup() {
            self.shutdown(event_loop, token);
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
