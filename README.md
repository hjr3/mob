# mob

A multi-echo server written in Rust using the mio async-io library.

[![Build Status](https://travis-ci.org/hjr3/mob.svg?branch=master)](https://travis-ci.org/hjr3/mob)

* `master` branch is currently setup to work against the master branch of mio (aka `0.6.0-dev`)
* `0.5` branch is setup to work against the `0.5` branch of mio

## Install

Run `cargo build` to build both `mob-server` and `mob-client`.

### Client

The client is just a very simple way to send a bunch of messages to the server.

### Logging

I use the `env_logger` crate. Logging can be turned on for mob-server with:
```
RUST_LOG=mob_server ./target/debug/mob-server  
```
If you want to see the log output from mio as well, you can do:
```
RUST_LOG=mob_server,mio ./target/debug/mob-server
```

## Docker

```
docker run --rm -it -v $(pwd):/source schickling/rust cargo run --bin mob-server
```
