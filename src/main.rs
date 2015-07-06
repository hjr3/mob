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

const SERVER: Token = Token(0);

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
    interest: Interest
}

impl Connection {
    fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,

            // new connections are hung up when they are first created
            interest: Interest::hup(),
        }
    }

    // note: if you are wondering why we do not store `event_loop` in
    // Connection, i believe it is because we only want to borrow the loop
    // for a short time and then end the borrow. if we stored it in Connection,
    // then the mio library could not mutate it further.
    //
    // idea: pretty sure we should use a closure here instead, like thread::scoped
    fn readable(&mut self, event_loop: &mut EventLoop<MyHandler>) -> io::Result<()> {
        // there are more advanced buffers we can create, but to reduce
        // cognitive load while we learn how this all works, use this
        // naive buffer
        let mut buf = [0u8; 1024];
        let mut echo = false;

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

                    // we read something, so set our echo flag to true. this
                    // means we will send this message back to our clients
                    echo = true;

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
                    self.interest.remove(Interest::readable());
                    break;
                }
            }
        }

        if echo {
            // TODO: handle EGAIN
            debug!("echoing back");
            self.sock.write_all(&buf).unwrap();
        }

        // we are EPOLLET, so the event_loop will deregister our connection before handing off to
        // us. now that we are done, re-register so we can read more later.
        // note: this reregister call is the return value of our readable function
        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }

    fn writable(&mut self, event_loop: &mut EventLoop<MyHandler>) -> io::Result<()> {
        // TODO: implement this
        Ok(())
    }
}

struct Server {
    // main socket for our server
    sock: TcpListener,
    
    // a list of connections _accepted_ by our server
    conns: Slab<Connection>
}

impl Server {
    fn accept(&mut self, event_loop: &mut EventLoop<MyHandler>) -> io::Result<()> {
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
        let token = self.conns.insert_with(|token| {
            let conn = Connection::new(sock, token);
            conn
        }).unwrap();

        // register the new connection with the event_loop. this will let our connection read
        // next tick.
        // TODO: this is leaky. make our connection register itself.
        event_loop.register_opt(&self.conns[token].sock, token, Interest::readable(), PollOpt::edge() | PollOpt::oneshot())
            .ok().expect("could not register socket with event loop");

        // yo, remember we are EPOLLET. this means even our SERVER needs to reregister.
        event_loop.reregister(&self.sock, SERVER, Interest::readable(), PollOpt::edge() | PollOpt::oneshot())
            .ok().expect("could not reregister SERVER socket with event loop");

        // we need to return io::Result because our Connection readable event returns io::Result,
        // which we really do want. down below, our event handler is doing a match and we need all
        // the match arms to have the same type.
        // TODO: make our match arm return Ok(()) ?
        Ok(())
    }

    // forward a readable event to the connection, identified by the token
    fn conn_readable(&mut self, event_loop: &mut EventLoop<MyHandler>, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);
        self.find_connection_by_token(token).readable(event_loop)
    }

    // forward a writable event to the connection, identified by the token
    fn conn_writable(&mut self, event_loop: &mut EventLoop<MyHandler>, token: Token) -> io::Result<()> {
        debug!("server conn writable; token={:?}", token);
        self.find_connection_by_token(token).writable(event_loop)
    }

    fn find_connection_by_token<'a>(&'a mut self, token: Token) -> &'a mut Connection {
        &mut self.conns[token]
    }
}

struct MyHandler {
    server: Server,
}

impl MyHandler {
    fn new(server: TcpListener) -> MyHandler {
        MyHandler {
            server: Server {
                sock: server,

                // SERVER is Token(0), so start after that
                // we can deal with a max of 128 connections
                conns: Slab::new_starting_at(Token(1), 128)
            }
        }
    }
}

impl Handler for MyHandler {
    type Timeout = ();
    type Message = ();

    fn readable(&mut self, event_loop: &mut EventLoop<MyHandler>, token: Token, hint: ReadHint) {
        // TODO: properly handle hup hint
        debug!("hint = {:?}", hint);

        match token {
            SERVER => self.server.accept(event_loop).unwrap(),

            // if our readable event handler received a token that is _not_ SERVER, then it must be
            // one of the connections in the slab.
            _ => self.server.conn_readable(event_loop, token).unwrap()
        };
    }

    fn writable(&mut self, event_loop: &mut EventLoop<MyHandler>, token: Token) {
        match token {
            SERVER => panic!("received writable for token 0"),
            _ => self.server.conn_writable(event_loop, token).unwrap()
        };
    }
}

fn main() {
    env_logger::init().unwrap();

    let addr: SocketAddr = FromStr::from_str("127.0.0.1:8000").unwrap();
    let server = TcpListener::bind(&addr).unwrap();

    let mut event_loop = EventLoop::new().unwrap();
    event_loop.register_opt(&server, SERVER, Interest::readable(), PollOpt::edge() | PollOpt::oneshot())
        .and_then(|_| {
            info!("Even loop starting...");
            Ok(())
        }).and_then(|_| {
            event_loop.run(&mut MyHandler::new(server))
        }).unwrap_or_else(|e| {
            panic!("{}", e)
        });
}
