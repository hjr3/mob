# mob

A multi-echo server written in Rust using the mio async-io library.

[![Build Status](https://travis-ci.org/hjr3/mob.svg?branch=multi-echo-blog-post)](https://travis-ci.org/hjr3/mob)

## Install

Run `cargo build` to build both `mob-server` and `mob-client`.

### Client

The client is just a very simple way to send a bunch of messages to the server.

### Logging

I use the `env_logger` create. Logging can be turned on for mob-server with `RUST_LOG=mob_server ./target/debug/mob-server`. If you want to see the log output from mio as well, you can do `RUST_LOG=mob_server,mio ./target/debug/mob-server`.
