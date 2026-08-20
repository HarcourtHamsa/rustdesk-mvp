//! rd-relay: the "AnyDesk-style" rendezvous point.
//!
//! Run this on a machine with a public IP (a small cloud VPS is the usual
//! choice). Both rd-host and rd-client connect *out* to it - the relay
//! never connects to either of them - so neither one needs a forwarded
//! port or a known IP. rd-host registers and gets a short code; rd-client
//! gives that code back to find the same host.
//!
//! The rendezvous logic itself lives in this crate's lib (`relay/src/lib.rs`)
//! so that `rd-host with-relay` can run the same code in-process when the
//! relay and the host happen to be the same machine.

use rd_protocol::DEFAULT_RELAY_PORT;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RELAY_PORT);

    let listener = rd_relay::bind(port).unwrap_or_else(|e| {
        eprintln!("Failed to bind relay port {port}: {e}");
        std::process::exit(1);
    });
    println!("rd-relay listening on 0.0.0.0:{port}");
    println!("Point rd-host and rd-client at this machine's public IP and this port.");

    rd_relay::serve(listener);
}
