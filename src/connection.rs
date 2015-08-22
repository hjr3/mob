use std::io;
use std::io::{Error, ErrorKind};

use mio::*;
use mio::tcp::*;
use bytes::ByteBuf;

use server::Server;

/// A stateful wrapper around a non-blocking stream. This connection is not
/// the SERVER connection. This connection represents the client connections
/// _accepted_ by the SERVER connection.
pub struct Connection {
    // handle to the accepted socket
    pub sock: TcpStream,

    // token used to register with the event loop
    pub token: Token,

    // set of events we are interested in
    interest: EventSet,

    // messages waiting to be sent out
    send_queue: Vec<ByteBuf>,

    is_reset: bool,
}

impl Connection {
    pub fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,

            // new connections are only listening for a hang up event when
            // they are first created. we always want to make sure we are
            // listening for the hang up event. we will additionally listen
            // for readable and writable events later on.
            interest: EventSet::hup(),

            send_queue: Vec::new(),

            is_reset: false,
        }
    }

    /// Handle read event from event loop.
    ///
    /// Currently only reads a max of 2048 bytes. Excess bytes are dropped on the floor.
    ///
    /// The recieve buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    pub fn readable(&mut self) -> io::Result<ByteBuf> {

        // ByteBuf is a heap allocated slice that mio supports internally. We use this as it does
        // the work of tracking how much of our slice has been used. I chose a capacity of 2048
        // after reading
        // https://github.com/carllerche/mio/blob/eed4855c627892b88f7ca68d3283cbc708a1c2b3/src/io.rs#L23-27
        // as that seems like a good size of streaming. If you are wondering what the difference
        // between messaged based and continuous streaming read
        // http://stackoverflow.com/questions/3017633/difference-between-message-oriented-protocols-and-stream-oriented-protocols
        // . TLDR: UDP vs TCP. We are using TCP.
        let mut recv_buf = ByteBuf::mut_with_capacity(2048);

        // we are PollOpt::edge() and PollOpt::oneshot(), so we _must_ drain
        // the entire socket receive buffer, otherwise the server will hang.
        loop {
            match self.sock.try_read_buf(&mut recv_buf) {
                // the socket receive buffer is empty, so let's move on
                // try_read_buf internally handles WouldBlock here too
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
                    return Err(e);
                }
            }
        }

        // change our type from MutByteBuf to ByteBuf
        Ok(recv_buf.flip())
    }

    /// Handle a writable event from the event loop.
    ///
    /// Send one message from the send queue to the client. If the queue is empty, remove interest
    /// in write events.
    /// TODO: Figure out if sending more than one message is optimal. Maybe we should be trying to
    /// flush until the kernel sends back EAGAIN?
    pub fn writable(&mut self) -> io::Result<()> {

        try!(self.send_queue.pop()
            .ok_or(Error::new(ErrorKind::Other, "Could not pop send queue"))
            .and_then(|mut buf| {
                match self.sock.try_write_buf(&mut buf) {
                    Ok(None) => {
                        debug!("client flushing buf; WouldBlock");

                        // put message back into the queue so we can try again
                        self.send_queue.push(buf);
                        Ok(())
                    },
                    Ok(Some(n)) => {
                        debug!("CONN : we wrote {} bytes", n);
                        Ok(())
                    },
                    Err(e) => {
                        error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                        Err(e)
                    }
                }
            })
        );

        if self.send_queue.is_empty() {
            self.interest.remove(EventSet::writable());
        }

        Ok(())
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the event loop.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    pub fn send_message(&mut self, message: ByteBuf) -> io::Result<()> {
        self.send_queue.push(message);
        self.interest.insert(EventSet::writable());
        Ok(())
    }

    /// Register interest in read events with the event_loop.
    ///
    /// This will let our connection accept reads starting next event loop tick.
    pub fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        self.interest.insert(EventSet::readable());

        event_loop.register_opt(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        ).or_else(|e| {
            error!("Failed to reregister {:?}, {:?}", self.token, e);
            Err(e)
        })
    }

    /// Re-register interest in read events with the event_loop.
    pub fn reregister(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        event_loop.reregister(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        ).or_else(|e| {
            error!("Failed to reregister {:?}, {:?}", self.token, e);
            Err(e)
        })
    }

    pub fn mark_reset(&mut self) {
        self.is_reset = true;
    }

    pub fn is_reset(&self) -> bool {
        self.is_reset
    }
}
