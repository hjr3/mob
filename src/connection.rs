use std::io;
use std::io::prelude::*;
use std::io::{Cursor, Error, ErrorKind};

use byteorder::{ByteOrder, BigEndian};

use mio::*;
use mio::tcp::*;

use server::Server;

/// A stateful wrapper around a non-blocking stream. This connection is not
/// the SERVER connection. This connection represents the client connections
/// _accepted_ by the SERVER connection.
pub struct Connection {
    // handle to the accepted socket
    sock: TcpStream,

    // token used to register with the event loop
    pub token: Token,

    // set of events we are interested in
    interest: EventSet,

    // messages waiting to be sent out
    send_queue: Vec<Vec<u8>>,

    is_reset: bool,

    read_continuation: Option<u64>,

}

impl Connection {
    pub fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,
            interest: EventSet::hup(),
            send_queue: Vec::new(),
            is_reset: false,
            read_continuation: None,
        }
    }

    /// Handle read event from event loop.
    ///
    /// The Handler must continue calling until no more ByteBuf is returned.
    ///
    /// The recieve buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    pub fn readable(&mut self) -> io::Result<Option<Vec<u8>>> {

        let msg_len = match try!(self.read_message_length()) {
            None => { return Ok(None); },
            Some(n) => n,
        };

        if msg_len == 0 {
            debug!("message is zero bytes; token={:?}", self.token);
            return Ok(None);
        }

        debug!("Expected message length: {}", msg_len);
        let mut recv_buf : Vec<u8> = Vec::with_capacity(msg_len as usize);

        // resolve "multiple applicable items in scope [E0034]" error
        let sock_ref = <TcpStream as Read>::by_ref(&mut self.sock);

        match sock_ref.take(msg_len as u64).try_read_buf(&mut recv_buf) {
            Ok(None) => {
                debug!("CONN : read encountered WouldBlock");

                // We are being forced to try again, but we already read the two bytes off of the
                // wire that determined the length. We need to store the message length so we can
                // resume next time we get readable.
                self.read_continuation = Some(msg_len as u64);
                Ok(None)
            },
            Ok(Some(n)) => {
                debug!("CONN : we read {} bytes", n);

                if n < msg_len as usize {
                    return Err(Error::new(ErrorKind::InvalidData, "Did not read enough bytes"));
                }

                self.read_continuation = None;

                Ok(Some(recv_buf))
            },
            Err(e) => {
                error!("Failed to read buffer for token {:?}, error: {}", self.token, e);
                Err(e)
            }
        }
    }

    fn read_message_length(&mut self) -> io::Result<Option<u64>> {
        if let Some(n) = self.read_continuation {
            return Ok(Some(n));
        }

        let mut buf = [0u8; 8];

        let bytes = match self.sock.try_read(&mut buf) {
            Ok(None) => {
                return Ok(None);
            },
            Ok(Some(n)) => n,
            Err(e) => {
                return Err(e);
            }
        };

        if bytes < 8 {
            warn!("Found message length of {} bytes", bytes);
            return Err(Error::new(ErrorKind::InvalidData, "Invalid message length"));
        }

        let msg_len = BigEndian::read_u64(buf.as_ref());
        Ok(Some(msg_len))
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
            .and_then(|buf| {
                match self.write_message_length(&buf) {
                    Ok(None) => {
                        // put message back into the queue so we can try again
                        self.send_queue.push(buf);
                        return Ok(());
                    },
                    Ok(Some(())) => {
                        ()
                    },
                    Err(e) => {
                        error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                        return Err(e);
                    }
                }

                let mut send_buf = Cursor::new(buf);
                match self.sock.try_write_buf(&mut send_buf) {
                    Ok(None) => {
                        debug!("client flushing buf; WouldBlock");

                        // put message back into the queue so we can try again
                        self.send_queue.push(send_buf.into_inner());
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

    fn write_message_length(&mut self, buf: &Vec<u8>) -> io::Result<Option<()>> {

        let len = buf.len();
        let mut send_buf = [0u8; 8];
        BigEndian::write_u64(&mut send_buf, len as u64);

        match self.sock.try_write(&mut send_buf) {
            Ok(None) => {
                debug!("client flushing buf; WouldBlock");

                Ok(None)
            },
            Ok(Some(n)) => {
                debug!("Sent message length of {} bytes", n);
                Ok(Some(()))
            },
            Err(e) => {
                error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                Err(e)
            }
        }
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the event loop.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    pub fn send_message(&mut self, message: Vec<u8>) -> io::Result<()> {
        trace!("connection send_message; token={:?}", self.token);

        self.send_queue.push(message);

        if !self.interest.is_writable() {
            self.interest.insert(EventSet::writable());
        }

        Ok(())
    }

    /// Register interest in read events with the event_loop.
    ///
    /// This will let our connection accept reads starting next event loop tick.
    pub fn register(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        trace!("connection register; token={:?}", self.token);

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
        trace!("connection reregister; token={:?}", self.token);

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

    //pub fn deregister(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //    trace!("connection deregister; token={:?}", self.token);

    //    event_loop.deregister(
    //        &self.sock
    //    ).or_else(|e| {
    //        error!("Failed to deregister {:?}, {:?}", self.token, e);
    //        Err(e)
    //    })
    //}

    pub fn mark_reset(&mut self) {
        trace!("connection mark_reset; token={:?}", self.token);

        self.is_reset = true;
    }

    pub fn is_reset(&self) -> bool {
        self.is_reset
    }
}
