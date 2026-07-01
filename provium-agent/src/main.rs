//! `provium-agent` binary entry point.
//!
//! Parses CLI arguments (port number; defaults to 1234), binds a
//! [`vsock::VsockListener`] on the configured port and `VMADDR_CID_ANY`,
//! then accepts connections in a loop, spawning a handler thread per
//! accept. Errors on individual connections are logged to stderr but
//! do not bring down the agent — only listener-level failures (the
//! socket itself dying) cause exit.

use std::env;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process;
use std::sync::Arc;
use std::thread;

use vsock::{VsockListener, VMADDR_CID_ANY};

use provium_agent::connection::{handle_connection, serve_worker_child, ConnectionOutcome};
use provium_agent::{init, AgentState};

const DEFAULT_PORT: u32 = 1234;

fn main() {
    // Worker (sub-agent) mode: launched by a parent agent as
    // `provium-agent --worker-fd N`, where N is the child end of a
    // socketpair the parent relays ops over. We are a plain child
    // process — not PID 1, no vsock — so we skip init duties and the
    // listener entirely and just serve the control channel until the
    // parent closes it. This is the whole mechanism behind a worker
    // having its own kernel credentials (token/PSB/privileges).
    if let Some(fd) = worker_fd_from_args() {
        // SAFETY: the parent passed us this fd's number across exec and
        // cleared FD_CLOEXEC on it; it is an open AF_UNIX stream that we
        // now own exclusively.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        match serve_worker_child(stream) {
            Ok(()) => process::exit(0),
            Err(e) => {
                eprintln!("provium-agent (worker): {e}");
                process::exit(1);
            }
        }
    }

    // If we're PID 1 (booted as `/init` from an initrd), perform the
    // standard mount sequence before doing anything else. No-op when
    // running under a real init system that has already mounted these.
    init::run_if_init();

    let port = match parse_port_from_env_or_args() {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("provium-agent: {msg}");
            process::exit(2);
        }
    };

    let listener = match VsockListener::bind_with_cid_port(VMADDR_CID_ANY, port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("provium-agent: bind on vsock port {port} failed: {e}");
            process::exit(1);
        }
    };

    let state = Arc::new(AgentState::new());
    eprintln!("provium-agent: listening on vsock port {port}");

    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                let state = Arc::clone(&state);
                let _ = thread::Builder::new()
                    .name(format!("agent-{}", addr.cid()))
                    .spawn(move || serve_connection(stream, state));
            }
            Err(e) => {
                eprintln!("provium-agent: accept failed: {e}");
                // Continue — accept failures are typically transient
                // (signal interruption, etc.). A persistent failure
                // would only surface as repeated log lines.
            }
        }
    }
}

fn serve_connection(mut stream: vsock::VsockStream, state: Arc<AgentState>) {
    // Clone the stream so handle_connection can take separate &mut R
    // and &mut W; on a vsock stream both halves share the underlying
    // socket so clone() is just an FD-level dup.
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("provium-agent: try_clone failed: {e}");
            return;
        }
    };

    match handle_connection(&mut reader, &mut stream, state) {
        Ok(ConnectionOutcome::Handled) => {}
        Ok(ConnectionOutcome::VersionRejected { host_version }) => {
            eprintln!(
                "provium-agent: rejected host version {host_version} (agent is v{})",
                provium_protocol::PROTOCOL_VERSION,
            );
        }
        Err(e) => {
            eprintln!("provium-agent: connection error: {e}");
        }
    }
}

/// Scan argv for `--worker-fd N`, returning the raw fd if present. A
/// malformed value aborts the process: a worker that can't find its
/// control channel has nothing useful to do.
fn worker_fd_from_args() -> Option<RawFd> {
    let mut args = env::args();
    let _ = args.next(); // program name
    while let Some(a) = args.next() {
        if a == "--worker-fd" {
            let v = args.next().unwrap_or_else(|| {
                eprintln!("provium-agent: --worker-fd requires a value");
                process::exit(2);
            });
            return Some(v.parse::<RawFd>().unwrap_or_else(|e| {
                eprintln!("provium-agent: --worker-fd: {e}");
                process::exit(2);
            }));
        }
    }
    None
}

fn parse_port_from_env_or_args() -> Result<u32, String> {
    // Argument form: `provium-agent --port N`
    let mut args = env::args();
    let _ = args.next(); // program name
    let mut port: Option<u32> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_owned())?;
                port = Some(v.parse::<u32>().map_err(|e| format!("--port: {e}"))?);
            }
            "--help" | "-h" => {
                println!("usage: provium-agent [--port N]");
                println!("default port: {DEFAULT_PORT}");
                process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if let Some(p) = port {
        return Ok(p);
    }

    // Env-var form: PROVIUM_AGENT_PORT.
    if let Ok(v) = env::var("PROVIUM_AGENT_PORT") {
        return v
            .parse::<u32>()
            .map_err(|e| format!("PROVIUM_AGENT_PORT: {e}"));
    }

    Ok(DEFAULT_PORT)
}
