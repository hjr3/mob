use std::collections::VecDeque;
use std::io;
use std::io::prelude::*;
use std::io::{Error, ErrorKind};
use std::rc::Rc;

use byteorder::{ByteOrder, BigEndian};

use mio::{Poll, PollOpt, Ready, Token};
use mio::net::TcpStream;
use mio::unix::UnixReady;

/// A stateful wrapper around a non-blocking stream. This connection is not
/// the SERVER connection. This connection represents the client connections
/// _accepted_ by the SERVER connection.
pub struct Connection {
    // handle to the accepted socket
    sock: TcpStream,

    // token used to register with the poller
    pub token: Token,

    // set of events we are interested in
    interest: Ready,

    // messages waiting to be sent out
    send_queue: VecDeque<Rc<Vec<u8>>>,

    // track whether a read received `WouldBlock` and store the number of
    // byte we are supposed to read
    read_continuation: Option<u64>,

    // track whether a write received `WouldBlock`
    write_continuation: bool,

}

impl Connection {
    pub fn new(sock: TcpStream, token: Token) -> Connection {
        Connection {
            sock: sock,
            token: token,
            interest: Ready::from(UnixReady::hup()),
            send_queue: VecDeque::with_capacity(32),
            read_continuation: None,
            write_continuation: false,
        }
    }

    /// Handle read event from poller.
    ///
    /// The Handler must continue calling until None is returned.
    ///
    /// The recieve buffer is sent back to `Server` so the message can be broadcast to all
    /// listening connections.
    pub fn readable(&mut self) -> io::Result<Option<Vec<u8>>> {

        let msg_len = match self.read_message_length()? {
            None => { return Ok(None); },
            Some(n) => n,
        };

        if msg_len == 0 {
            debug!("message is zero bytes; token={:?}", self.token);
            return Ok(None);
        }

        let msg_len = msg_len as usize;

        debug!("Expected message length is {}", msg_len);

        // Here we allocate and set the length with unsafe code. The risks of this are discussed
        // at https://stackoverflow.com/a/30979689/329496 and are mitigated as recv_buf is
        // abandoned below if we don't read msg_leg bytes from the socket
        let mut recv_buf : Vec<u8> = Vec::with_capacity(msg_len);
        unsafe { recv_buf.set_len(msg_len); }

        // UFCS: resolve "multiple applicable items in scope [E0034]" error
        let sock_ref = <TcpStream as Read>::by_ref(&mut self.sock);

        match sock_ref.take(msg_len as u64).read(&mut recv_buf) {
            Ok(n) => {
                debug!("CONN : we read {} bytes", n);

                // TODO handle a read continuation here
                if n < msg_len as usize {
                    return Err(Error::new(ErrorKind::InvalidData, "Did not read enough bytes"));
                }

                self.read_continuation = None;

                Ok(Some(recv_buf.to_vec()))
            }
            Err(e) => {

                if e.kind() == ErrorKind::WouldBlock {
                    debug!("CONN : read encountered WouldBlock");

                    // We are being forced to try again, but we already read the two bytes off of the
                    // wire that determined the length. We need to store the message length so we can
                    // resume next time we get readable.
                    self.read_continuation = Some(msg_len as u64);
                    Ok(None)
                } else {
                    error!("Failed to read buffer for token {:?}, error: {}", self.token, e);
                    Err(e)
                }
            }
        }
    }

    fn read_message_length(&mut self) -> io::Result<Option<u64>> {
        if let Some(n) = self.read_continuation {
            return Ok(Some(n));
        }

        let mut buf = [0u8; 8];

        let bytes = match self.sock.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    return Ok(None);
                } else {
                    return Err(e);
                }
            }
        };

        if bytes < 8 {
            warn!("Found message length of {} bytes", bytes);
            return Err(Error::new(ErrorKind::InvalidData, "Invalid message length"));
        }

        let msg_len = BigEndian::read_u64(buf.as_ref());
        Ok(Some(msg_len))
    }

    /// Handle a writable event from the poller.
    ///
    /// Send one message from the send queue to the client. If the queue is empty, remove interest
    /// in write events.
    /// TODO: Figure out if sending more than one message is optimal. Maybe we should be trying to
    /// flush until the kernel sends back EAGAIN?
    pub fn writable(&mut self) -> io::Result<()> {

        self.send_queue.pop_front()
            .ok_or(Error::new(ErrorKind::Other, "Could not pop send queue"))
            .and_then(|buf| {
                self.write_message(buf)
            })?;

        if self.send_queue.is_empty() {
            self.interest.remove(Ready::writable());
        }

        Ok(())
    }

    fn write_message_length(&mut self, buf: &Rc<Vec<u8>>) -> io::Result<Option<()>> {
        if self.write_continuation {
            return Ok(Some(()));
        }

        let len = buf.len();
        let mut send_buf = [0u8; 8];
        BigEndian::write_u64(&mut send_buf, len as u64);

        let len = send_buf.len();
        match self.sock.write(&send_buf) {
            Ok(n) => {
                if n < len {
                    let e = Error::new(ErrorKind::Other, "Message length failed");
                    error!("Failed to send message length for {:?}, error: {}", self.token, e);
                    Err(e)
                } else {
                    debug!("Sent message length of {} bytes", n);
                    Ok(Some(()))
                }
            }
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    debug!("client flushing buf; WouldBlock");

                    Ok(None)
                } else {
                    error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                    Err(e)
                }
            }
        }
    }

    fn write_message(&mut self, buf: Rc<Vec<u8>>) -> io::Result<()> {
        match self.write_message_length(&buf) {
            Ok(None) => {
                // put message back into the queue so we can try again
                self.send_queue.push_front(buf);
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

        let len = buf.len();
        match self.sock.write(&*buf) {
            Ok(n) => {
                debug!("CONN : we wrote {} bytes", n);
                // if we wrote a partial message, then put remaining part of message back
                // into the queue so we can try again
                if n < len {
                    let remaining = Rc::new(buf[n..].to_vec());
                    self.send_queue.push_front(remaining);
                    self.write_continuation = true;
                } else {
                    self.write_continuation = false;
                }
                Ok(())
            },
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    debug!("client flushing buf; WouldBlock");

                    // put message back into the queue so we can try again
                    self.send_queue.push_front(buf);
                    self.write_continuation = true;
                    Ok(())
                } else {
                    error!("Failed to send buffer for {:?}, error: {}", self.token, e);
                    Err(e)
                }
            }
        }
    }

    /// Queue an outgoing message to the client.
    ///
    /// This will cause the connection to register interests in write events with the poller.
    /// The connection can still safely have an interest in read events. The read and write buffers
    /// operate independently of each other.
    pub fn send_message(&mut self, message: Rc<Vec<u8>>) -> io::Result<()> {
        trace!("connection send_message; token={:?}", self.token);

        // if the queue is empty then try and write. if we get WouldBlock the message will get
        // queued up for later. if the queue already has items in it, then we know that we got
        // WouldBlock from a previous write, so queue it up and wait for the next write event.
        if self.send_queue.is_empty() {
            self.write_message(message)?;
        } else {
            self.send_queue.push_back(message);
        }

        if !self.send_queue.is_empty() && !self.interest.is_writable() {
            self.interest.insert(Ready::writable());
        }

        Ok(())
    }

    /// Register interest in read events with poll.
    ///
    /// This will let our connection accept reads starting next poller tick.
    pub fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        trace!("connection register; token={:?}", self.token);

        self.interest.insert(Ready::readable());

        poll.register(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        ).or_else(|e| {
            error!("Failed to reregister {:?}, {:?}", self.token, e);
            Err(e)
        })
    }

    /// Re-register interest in read events with poll.
    pub fn reregister(&mut self, poll: &mut Poll) -> io::Result<()> {
        trace!("connection reregister; token={:?}", self.token);

        poll.reregister(
            &self.sock,
            self.token,
            self.interest,
            PollOpt::edge() | PollOpt::oneshot()
        ).or_else(|e| {
            error!("Failed to reregister {:?}, {:?}", self.token, e);
            Err(e)
        })
    }
}
