extern crate byteorder;

use std::io::prelude::*;
use std::net::TcpStream;
use std::thread;

use byteorder::{ByteOrder, BigEndian};

static NTHREADS: i32 = 2;

fn main() {

    for i in 0..NTHREADS {

        let _ = thread::spawn(move|| {

            let mut stream = TcpStream::connect("127.0.0.1:8000").unwrap();

            loop {
                let msg = format!("the answer is {}", i);
                let mut buf = [0u8; 8];

                println!("Sending over message length of {}", msg.len());
                BigEndian::write_u64(&mut buf, msg.len() as u64);
                stream.write_all(buf.as_ref()).unwrap();
                stream.write_all(msg.as_ref()).unwrap();

                let mut r = [0u8; 256];
                match stream.read(&mut r) {
                    Ok(0) => {
                        println!("thread {}: 0 bytes read", i);
                    },
                    Ok(n) => {
                        println!("thread {}: {} bytes read", i, n);

                        let s = std::str::from_utf8(&r[..]).unwrap();
                        println!("thread {} read = {}", i, s);
                    },
                    Err(e) => {
                        panic!("thread {}: {}", i, e);
                    }
                }
            }
        });
    }

    loop {}
}
