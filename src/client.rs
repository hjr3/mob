use std::io::prelude::*;
use std::net::TcpStream;
use std::thread;

static NTHREADS: i32 = 10;

fn main() {

    for i in 0..NTHREADS {

        let _ = thread::spawn(move|| {

            let mut stream = TcpStream::connect("127.0.0.1:8000").unwrap();

            loop {
                write!(stream, "the answer is {}", i).unwrap();

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
