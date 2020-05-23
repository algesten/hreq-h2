# hreq-h2

A Tokio un-aware, HTTP/2.0 client & server implementation for Rust.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

This is the [h2] crate with some modification to remove dependencies on tokio. This roughly means:

* tokio's [`AsyncWrite`] and [`AsyncRead`] are replaced with the standard 
  variants from the [futures crate]. The potential optimizations tokio aims for are lost.
* Copy tokio's [`codec`] into the source tree.

The modifications are made in step-by-step commits to try and clearly illustrate how to redo
the changes as the original crate updates.

Publishing this crate is by no means an attempt to take credit for or criticise the excellent
work done by people behind h2/hyperium/tokio.

[h2]: https://crates.io/crates/h2
[`AsyncWrite`]: https://docs.rs/tokio/latest/tokio/io/trait.AsyncWrite.html
[`AsyncRead`]: https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html
[futures crate]: https://docs.rs/futures/latest/futures/io/index.html
[`codec`]: https://docs.rs/tokio-util/latest/tokio_util/codec/index.html
