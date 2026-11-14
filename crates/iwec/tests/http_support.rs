//! Shared HTTP-server spawn helpers for the integration tests.
//!
//! `free_port()` binds `:0` and drops the listener, so between the drop and
//! the child's own bind another process (or another test's connection) may
//! take the port. The child then exits on a bind failure while a plain
//! "can I connect" readiness check would happily succeed against whoever
//! owns the port. These helpers prove the listener is *our* child: it must
//! be alive both before and after a successful TCP connect, and a child
//! that exited is respawned (bounded retries).

use std::collections::HashSet;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ATTEMPTS: usize = 8;
const READY_TIMEOUT: Duration = Duration::from_secs(15);

static HANDED_OUT: Mutex<Option<HashSet<u16>>> = Mutex::new(None);

/// An ephemeral port no earlier `free_port()` call in this process returned.
pub fn free_port() -> u16 {
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        let mut guard = HANDED_OUT.lock().unwrap_or_else(|e| e.into_inner());
        if guard.get_or_insert_with(HashSet::new).insert(port) {
            return port;
        }
    }
}

enum Ready {
    Listening,
    Exited,
}

fn wait_ready(child: &mut Child, port: u16) -> Ready {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ready::Exited;
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            // The connect may have reached a foreign listener while our
            // child was dying on a bind failure: re-check liveness.
            std::thread::sleep(Duration::from_millis(50));
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ready::Exited;
            }
            return Ready::Listening;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not start listening on 127.0.0.1:{port}");
}

/// Pick a fresh port, spawn via `spawn(port)`, and return once the child is
/// verified to be the listener. A child that exits (bind failure) is
/// respawned on a new port.
pub fn spawn_on_free_port<F: FnMut(u16) -> Child>(mut spawn: F) -> (u16, Child) {
    for _ in 0..ATTEMPTS {
        let port = free_port();
        let mut child = spawn(port);
        if let Ready::Listening = wait_ready(&mut child, port) {
            return (port, child);
        }
        let _ = child.wait();
    }
    panic!("iwec exited on every one of {ATTEMPTS} port attempts");
}

/// Like `spawn_on_free_port` for a caller-fixed `port` (restarts on the same
/// port): retries the same port after a short pause.
pub fn spawn_on_port<F: FnMut() -> Child>(port: u16, mut spawn: F) -> Child {
    for _ in 0..ATTEMPTS {
        let mut child = spawn();
        if let Ready::Listening = wait_ready(&mut child, port) {
            return child;
        }
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("iwec exited on every one of {ATTEMPTS} attempts on port {port}");
}
