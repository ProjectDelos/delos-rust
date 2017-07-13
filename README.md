# delos-rust
## To Build
Download and install [Rust](https://www.rust-lang.org) (easiest way is `curl https://sh.rustup.rs -sSf | sh`).  
Clone this repository.  
To run local tests

    cargo test --release

**NOTE:** The first build will be much slower than subsequent ones
as it needs to download dependencies.

## Servers

A CLI binding for starting Fuzzy Log servers can be found in [servers/tcp_server].

## C Bindings

C bindings are currently located in [examples/c_linking](examples/c_linking).
Examples and build instructions for C applications can be found there

## Directory Outline
[src](src) fuzzy log client library  
[example](examples)` sample code which uses the client library to perform vaious tasks of note is  
[examples/c_linking](examples/c_linking) shows how to use the C API to interface with the fuzzy log client library  
[servers](servers) various servers which the client library can run against including  
[servers/tcp_server](servers/tcp_server) a TCP based sever  
[clients](clients) varous DPDK based clients for use in testing (largely obsolescent)  

[![Build Status](https://travis-ci.com/ProjectDelos/delos-rust.svg?token=RaEyYb9eyzdWqhSpjYxi&branch=mahesh_look_at_this)](https://travis-ci.com/ProjectDelos/delos-rust)
