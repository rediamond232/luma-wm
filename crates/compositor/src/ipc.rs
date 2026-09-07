//! Bounded local IPC. Socket ownership is exclusive; never unlink another live session.
use smithay::reexports::calloop::channel::Sender;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use wm_core::{Request, Response, Snapshot};
type Commands = Sender<(String, Option<mpsc::SyncSender<Result<(), String>>>)>;
pub fn serve(
    tx: Commands,
    state: Arc<Mutex<Snapshot>>,
    subscribers: Arc<Mutex<Vec<mpsc::SyncSender<Snapshot>>>>,
) -> Result<(), String> {
    let path = wm_core::socket_path()?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(format!("refusing to replace non-socket {}", path.display()));
            }
            match UnixStream::connect(&path) {
                Ok(_) => return Err(format!("another compositor owns {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            if live.load(std::sync::atomic::Ordering::Relaxed) >= 32 {
                continue;
            }
            live.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (live, tx, state, subscribers) =
                (live.clone(), tx.clone(), state.clone(), subscribers.clone());
            std::thread::spawn(move || {
                handle(stream, tx, state, subscribers);
                live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });
    Ok(())
}
fn handle(
    mut stream: UnixStream,
    tx: Commands,
    state: Arc<Mutex<Snapshot>>,
    subscribers: Arc<Mutex<Vec<mpsc::SyncSender<Snapshot>>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let Ok(copy) = stream.try_clone() else { return };
    if BufReader::new(copy)
        .take(65537)
        .read_line(&mut line)
        .is_err()
        || line.len() > 65536
    {
        return;
    }
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(_) => return,
    };
    if req.version != 1 {
        send(
            &mut stream,
            &state,
            Err("unsupported protocol version".into()),
        );
        return;
    }
    if req.command == "subscribe" {
        let (s, r) = mpsc::sync_channel(8);
        subscribers.lock().unwrap().push(s);
        send(&mut stream, &state, Ok(()));
        loop {
            // A heartbeat releases disconnected clients even while the desktop is idle.
            // Always read current state so a saturated queue cannot leave clients stale.
            match r.recv_timeout(Duration::from_secs(2)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                _ => {}
            }
            while r.try_recv().is_ok() {}
            let snapshot = state.lock().unwrap().clone();
            let response = Response {
                version: 1,
                ok: true,
                error: None,
                state: snapshot,
            };
            if serde_json::to_writer(&mut stream, &response).is_err()
                || stream.write_all(b"\n").is_err()
            {
                break;
            }
        }
    } else {
        let (s, r) = mpsc::sync_channel(1);
        if tx.send((req.command, Some(s))).is_err() {
            return;
        }
        let result = r
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| Err("compositor did not respond".into()));
        send(&mut stream, &state, result);
    }
}
fn send(stream: &mut UnixStream, state: &Arc<Mutex<Snapshot>>, result: Result<(), String>) {
    let r = Response {
        version: 1,
        ok: result.is_ok(),
        error: result.err(),
        state: state.lock().unwrap().clone(),
    };
    let _ = serde_json::to_writer(&mut *stream, &r);
    let _ = stream.write_all(b"\n");
}
