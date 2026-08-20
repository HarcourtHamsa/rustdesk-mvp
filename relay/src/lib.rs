//! The rendezvous logic behind rd-relay, as a library.
//!
//! This lives in a lib rather than straight in `main.rs` so it can be run
//! two ways from the same code:
//!
//! - as the standalone `rd-relay` binary on a VPS (the normal deployment), and
//! - inside the rd-host process, for `rd-host with-relay`, when the relay
//!   and the host are the same machine and you'd rather run one command.
//!
//! Once a client and a host are matched for a session, the relay stops
//! looking at the traffic at all - it just copies bytes in both
//! directions. It cannot decrypt anything because there's nothing to
//! decrypt: this MVP protocol isn't encrypted end-to-end, which means the
//! operator of the relay CAN see the raw traffic if they wanted to. Run it
//! somewhere you trust, same as you would the host machine itself. See the
//! top-level README for the SSH tunneling option if you need real
//! confidentiality.

use rd_protocol::{
    generate_code, read_message, write_message, RelayHello, RelayToClient, RelayToHost,
};
use std::collections::HashMap;
use std::io::{Read, Result as IoResult};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct HostEntry {
    /// A clone of the host's control-connection socket, used only to write
    /// `IncomingSession` notices to it.
    control_writer: TcpStream,
}

type HostRegistry = Arc<Mutex<HashMap<String, HostEntry>>>;
type PendingSessions = Arc<Mutex<HashMap<u64, mpsc::Sender<TcpStream>>>>;

/// Claim the relay port. Kept separate from [`serve`] so a caller that
/// starts the relay on a background thread can still surface a bind failure
/// (port in use, permission denied) synchronously, before it commits to
/// anything else.
pub fn bind(port: u16) -> IoResult<TcpListener> {
    TcpListener::bind(("0.0.0.0", port))
}

/// Accept and handle relay connections forever. Only returns if the
/// listener itself dies.
pub fn serve(listener: TcpListener) {
    let hosts: HostRegistry = Arc::new(Mutex::new(HashMap::new()));
    let pending: PendingSessions = Arc::new(Mutex::new(HashMap::new()));

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[relay] accept error: {e}");
                continue;
            }
        };
        stream.set_nodelay(true).ok();
        let hosts = hosts.clone();
        let pending = pending.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, hosts, pending) {
                eprintln!("[relay] connection error: {e}");
            }
        });
    }
}

fn handle_connection(
    mut stream: TcpStream,
    hosts: HostRegistry,
    pending: PendingSessions,
) -> IoResult<()> {
    let hello: RelayHello = read_message(&mut stream)?;
    match hello {
        RelayHello::RegisterHost => handle_register_host(stream, hosts),
        RelayHello::ConnectByCode { code } => handle_connect_by_code(stream, code, hosts, pending),
        RelayHello::SessionData { session_id } => handle_session_data(stream, session_id, pending),
    }
}

fn handle_register_host(mut stream: TcpStream, hosts: HostRegistry) -> IoResult<()> {
    let code = {
        let mut guard = hosts.lock().unwrap();
        loop {
            let candidate = generate_code();
            if !guard.contains_key(&candidate) {
                guard.insert(
                    candidate.clone(),
                    HostEntry {
                        control_writer: stream.try_clone()?,
                    },
                );
                break candidate;
            }
        }
    };
    println!("[relay] host registered with code {code}");
    write_message(
        &mut stream,
        &RelayToHost::Registered { code: code.clone() },
    )?;

    // Block here purely to detect when the host goes away, so the code can
    // be freed immediately rather than staying claimed until some timeout.
    let mut discard = [0u8; 64];
    loop {
        match stream.read(&mut discard) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue, // host isn't expected to send more; ignore stray bytes
        }
    }
    hosts.lock().unwrap().remove(&code);
    println!("[relay] host with code {code} disconnected");
    Ok(())
}

fn handle_connect_by_code(
    mut stream: TcpStream,
    code: String,
    hosts: HostRegistry,
    pending: PendingSessions,
) -> IoResult<()> {
    let mut control_writer = {
        let guard = hosts.lock().unwrap();
        match guard.get(&code) {
            Some(entry) => entry.control_writer.try_clone()?,
            None => {
                write_message(
                    &mut stream,
                    &RelayToClient::Error {
                        message: format!("no host is currently online with code {code}"),
                    },
                )?;
                return Ok(());
            }
        }
    };

    let session_id: u64 = rand::random();
    let (tx, rx) = mpsc::channel::<TcpStream>();
    pending.lock().unwrap().insert(session_id, tx);

    if write_message(&mut control_writer, &RelayToHost::IncomingSession { session_id }).is_err() {
        pending.lock().unwrap().remove(&session_id);
        write_message(
            &mut stream,
            &RelayToClient::Error {
                message: "host is no longer reachable".into(),
            },
        )?;
        return Ok(());
    }

    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(host_stream) => {
            println!("[relay] session {session_id} ({code}) matched - piping traffic");
            write_message(&mut stream, &RelayToClient::Connected)?;
            pipe(stream, host_stream);
        }
        Err(_) => {
            pending.lock().unwrap().remove(&session_id);
            write_message(
                &mut stream,
                &RelayToClient::Error {
                    message: "host did not respond in time".into(),
                },
            )?;
        }
    }
    Ok(())
}

fn handle_session_data(
    stream: TcpStream,
    session_id: u64,
    pending: PendingSessions,
) -> IoResult<()> {
    let sender = pending.lock().unwrap().remove(&session_id);
    if let Some(sender) = sender {
        // Hand the socket to the waiting client-handling thread; it takes it from here.
        let _ = sender.send(stream);
    }
    Ok(())
}

/// Full-duplex byte shovel between the client's socket and the host's data
/// socket. From here on the relay is blind to the rd-protocol traffic
/// flowing through it - it neither parses nor modifies it.
fn pipe(client_stream: TcpStream, host_stream: TcpStream) {
    let mut client_read = client_stream.try_clone().expect("clone client stream");
    let mut host_write = host_stream.try_clone().expect("clone host stream");
    let mut host_read = host_stream;
    let mut client_write = client_stream;

    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut host_write);
        let _ = host_write.shutdown(std::net::Shutdown::Both);
    });
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut host_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Both);
    });
    let _ = t1.join();
    let _ = t2.join();
}
