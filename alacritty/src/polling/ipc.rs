//! Alacritty socket IPC.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Read, Result as IoResult, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};

use log::{error, warn};
use std::result::Result;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::cli::{Options, SocketMessage};
use crate::event::{Event, EventType};

/// Environment variable name for the IPC socket path.
const ALACRITTY_SOCKET_ENV: &str = "ALACRITTY_SOCKET";

/// Maximum time to wait for a connected client to send its whole message.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum size of a single IPC message.
///
/// Messages are read on the shared `io event listener` thread, so an unbounded `read_line` would
/// let a client that never sends a newline grow `data` without limit.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// IPC socket listener.
pub struct IpcListener {
    pub socket: UnixListener,

    event_proxy: EventLoopProxy<Event>,
    data: String,
}

impl IpcListener {
    pub fn new(
        options: &Options,
        event_proxy: EventLoopProxy<Event>,
        path: &Path,
    ) -> Result<Self, IoError> {
        // Create unix socket in nonblocking mode.
        let socket = UnixListener::bind(path)?;
        socket.set_nonblocking(true)?;

        // Register socket path as environment variable for `alacritty msg`.
        unsafe { env::set_var(ALACRITTY_SOCKET_ENV, path.as_os_str()) };
        if options.daemon {
            println!("ALACRITTY_SOCKET={}; export ALACRITTY_SOCKET", path.display());
        }

        Ok(Self { event_proxy, socket, data: Default::default() })
    }

    /// Process the next IPC message.
    pub fn process_message(&mut self) -> Result<(), IoError> {
        // The poller can wake spuriously or after the client already gave up. `WouldBlock` is not a
        // failure, and propagating it would skip the signal pipe event in the same poll pass.
        let stream = match self.socket.accept() {
            Ok((stream, _)) => stream,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err),
        };

        // The poller wakes on connect, before the client has written its message, so the read
        // below must wait for it instead of failing with `WouldBlock`.
        if let Err(err) = prepare_stream(&stream) {
            warn!("Failed to configure IPC stream: {err}");
            return Ok(());
        }

        self.data.clear();

        match read_message(&stream, &mut self.data) {
            Ok(0) => return Ok(()),
            Err(err) => {
                warn!("Failed to read IPC message: {err}");
                return Ok(());
            },
            Ok(_) => (),
        };

        let message: SocketMessage = match serde_json::from_str(&self.data) {
            Ok(message) => message,
            Err(err) => {
                warn!("Failed to parse IPC message: {err}");
                return Ok(());
            },
        };

        // Handle IPC events.
        match message {
            SocketMessage::CreateWindow(options) => {
                let event = Event::new(EventType::CreateWindow(options), None);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::Config(ipc_config) => {
                let window_id =
                    ipc_config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcConfig(ipc_config), window_id);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::GetConfig(config) => {
                let window_id =
                    config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcGetConfig(Arc::new(stream)), window_id);
                let _ = self.event_proxy.send_event(event);
            },
        }

        Ok(())
    }
}

/// Read one IPC message, up to the first newline or the client closing its write end.
///
/// `READ_TIMEOUT` (set by `prepare_stream`) only bounds each `read` call, so this also enforces it
/// as a deadline for the whole message, and `MAX_MESSAGE_SIZE` caps its length. Without both, a
/// client that sends a byte every few hundred milliseconds without a newline would keep the shared
/// `io event listener` thread, which also drains the signal pipe. The deadline is checked after
/// every read, so a stalled message ends at most one `READ_TIMEOUT` after the deadline.
///
/// Returns the number of bytes appended to `data`.
fn read_message(mut stream: &UnixStream, data: &mut String) -> IoResult<usize> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut message = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read == 0 {
            break;
        }

        // Like `read_line`, keep everything up to and including the first newline.
        let newline = chunk[..read].iter().position(|&byte| byte == b'\n');
        let end = newline.map_or(read, |index| index + 1);
        message.extend_from_slice(&chunk[..end]);

        if message.len() > MAX_MESSAGE_SIZE {
            return Err(IoError::new(ErrorKind::InvalidData, "IPC message is too large"));
        }
        if newline.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(IoError::new(ErrorKind::TimedOut, "IPC message not completed in time"));
        }
    }

    let message =
        String::from_utf8(message).map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
    data.push_str(&message);
    Ok(message.len())
}

/// Put an accepted IPC stream into blocking mode with a bounded read.
///
/// On macOS/BSD an accepted socket inherits `O_NONBLOCK` from the nonblocking listener, so
/// `read_message` would get `WouldBlock` for a client that has connected but not yet written,
/// silently dropping its message. Linux does not inherit the flag, so this is a no-op there.
///
/// Messages are processed on the shared `io event listener` thread, which also drains the signal
/// pipe. The read timeout keeps a client that connects and never writes from stalling IPC and
/// signal handling; `read_message` adds the whole-message deadline and size cap. The timeout is set
/// once here: macOS rejects `setsockopt` with `EINVAL` once the peer has shut down its write end.
fn prepare_stream(stream: &UnixStream) -> IoResult<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))
}

/// Send a message to the active Alacritty socket.
pub fn send_message(socket: Option<PathBuf>, message: SocketMessage) -> IoResult<()> {
    let mut socket = find_socket(socket)?;

    // Write message to socket.
    let message_json = serde_json::to_string(&message)?;
    socket.write_all(message_json.as_bytes())?;
    let _ = socket.flush();

    // Shutdown write end, to allow reading.
    socket.shutdown(Shutdown::Write)?;

    // Get matching IPC reply.
    handle_reply(&socket, &message)?;

    Ok(())
}

/// Process IPC responses.
fn handle_reply(stream: &UnixStream, message: &SocketMessage) -> IoResult<()> {
    // Read reply, returning early if there is none.
    let mut buffer = String::new();
    let mut reader = BufReader::new(stream);
    if let Ok(0) | Err(_) = reader.read_line(&mut buffer) {
        return Ok(());
    }

    // Parse IPC reply.
    let reply: SocketReply = serde_json::from_str(&buffer)
        .map_err(|err| IoError::other(format!("Invalid IPC format: {err}")))?;

    // Ensure reply matches request.
    match (message, &reply) {
        // Write requested config to STDOUT.
        (SocketMessage::GetConfig(..), SocketReply::GetConfig(config)) => {
            println!("{config}");
            Ok(())
        },
        // Ignore requests without reply.
        _ => Ok(()),
    }
}

/// Send IPC message reply.
pub fn send_reply(stream: &mut UnixStream, message: SocketReply) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Failed to send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(stream: &mut UnixStream, message: SocketReply) -> IoResult<()> {
    let json = serde_json::to_string(&message).map_err(IoError::other)?;
    stream.write_all(json.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Directory for the IPC socket file.
#[cfg(not(target_os = "macos"))]
pub fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("alacritty")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(|path| fs::create_dir_all(&path).map(|_| path).ok())
        .unwrap_or_else(env::temp_dir)
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// Find the IPC socket path.
fn find_socket(socket_path: Option<PathBuf>) -> IoResult<UnixStream> {
    // Handle --socket CLI override.
    if let Some(socket_path) = socket_path {
        // Ensure we inform the user about an invalid path.
        return UnixStream::connect(&socket_path).map_err(|err| {
            let message = format!("invalid socket path {socket_path:?}");
            IoError::new(err.kind(), message)
        });
    }

    // Handle environment variable.
    if let Ok(path) = env::var(ALACRITTY_SOCKET_ENV) {
        let socket_path = PathBuf::from(path);
        if let Ok(socket) = UnixStream::connect(socket_path) {
            return Ok(socket);
        }
    }

    // Search for sockets files.
    for entry in fs::read_dir(socket_dir())?.filter_map(|entry| entry.ok()) {
        let path = entry.path();

        // Skip files that aren't Alacritty sockets.
        let socket_prefix = socket_prefix();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|file| file.starts_with(&socket_prefix) && file.ends_with(".sock"))
            .is_none()
        {
            continue;
        }

        // Attempt to connect to the socket.
        match UnixStream::connect(&path) {
            Ok(socket) => return Ok(socket),
            // Delete orphan sockets.
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(&path);
            },
            // Ignore other errors like permission issues.
            Err(_) => (),
        }
    }

    Err(IoError::new(ErrorKind::NotFound, "no socket found"))
}

/// File prefix matching all available sockets.
///
/// This prefix will include display server information to allow for environments with multiple
/// display servers running for the same user.
#[cfg(not(target_os = "macos"))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Alacritty-{}", display.replace('/', "-"))
}

/// File prefix matching all available sockets.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Alacritty")
}

/// IPC socket replies.
#[derive(Serialize, Deserialize, Debug)]
pub enum SocketReply {
    GetConfig(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::thread;

    /// Bind a nonblocking listener, like `IpcListener::new`, on a unique temporary path.
    fn bind_listener(name: &str) -> (UnixListener, PathBuf) {
        let path =
            env::temp_dir().join(format!("alacritty-ipc-test-{}-{name}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        (listener, path)
    }

    /// Accept the pending connection, like the poller wake-up in `process_message`.
    fn accept(listener: &UnixListener) -> UnixStream {
        loop {
            match listener.accept() {
                Ok((stream, _)) => return stream,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                },
                Err(err) => panic!("accept failed: {err}"),
            }
        }
    }

    #[test]
    fn prepared_stream_waits_for_late_client_write() {
        let (listener, path) = bind_listener("late");

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut socket = UnixStream::connect(client_path).unwrap();
            // Connect before writing, as the poller wakes on connect.
            thread::sleep(Duration::from_millis(100));
            socket.write_all(b"{\"CreateWindow\":null}").unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
        });

        let stream = accept(&listener);
        prepare_stream(&stream).unwrap();

        let mut data = String::new();
        let read = read_message(&stream, &mut data).unwrap();
        assert_eq!(read, data.len());
        assert_eq!(data, "{\"CreateWindow\":null}");

        client.join().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn prepared_stream_times_out_for_silent_client() {
        let (listener, path) = bind_listener("silent");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let client_path = path.clone();
        let client = thread::spawn(move || {
            let _socket = UnixStream::connect(client_path).unwrap();
            // Hold the connection open without writing until the server gave up.
            let _ = release_rx.recv();
        });

        let stream = accept(&listener);
        prepare_stream(&stream).unwrap();

        let mut data = String::new();
        let err = read_message(&stream, &mut data).unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut), "{err:?}");

        release_tx.send(()).unwrap();
        client.join().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_message_stops_at_first_newline() {
        let (listener, path) = bind_listener("newline");

        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut socket = UnixStream::connect(client_path).unwrap();
            // Let the server prepare the stream first, since macOS rejects `setsockopt` once the
            // peer has shut down.
            thread::sleep(Duration::from_millis(100));
            socket.write_all(b"{\"CreateWindow\":null}\nignored").unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
        });

        let stream = accept(&listener);
        prepare_stream(&stream).unwrap();

        let mut data = String::new();
        let read = read_message(&stream, &mut data).unwrap();
        assert_eq!(read, data.len());
        assert_eq!(data, "{\"CreateWindow\":null}\n");

        client.join().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_message_times_out_for_client_trickling_bytes() {
        let (listener, path) = bind_listener("trickle");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut socket = UnixStream::connect(client_path).unwrap();
            // A byte every 200 ms, never a newline: each read succeeds well inside the timeout.
            while release_rx.recv_timeout(Duration::from_millis(200)).is_err() {
                if socket.write_all(b"x").is_err() {
                    break;
                }
            }
        });

        let stream = accept(&listener);
        prepare_stream(&stream).unwrap();

        let started = Instant::now();
        let mut data = String::new();
        let err = read_message(&stream, &mut data).unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut), "{err:?}");
        assert!(started.elapsed() < READ_TIMEOUT * 3, "{:?}", started.elapsed());

        release_tx.send(()).unwrap();
        client.join().unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_message_rejects_oversized_message() {
        let (listener, path) = bind_listener("oversized");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let client_path = path.clone();
        let client = thread::spawn(move || {
            let mut socket = UnixStream::connect(client_path).unwrap();
            // More than the cap and no newline; the server gives up and the write may fail.
            let _ = socket.write_all(&vec![b'x'; MAX_MESSAGE_SIZE + 4096]);
            let _ = release_rx.recv();
        });

        let stream = accept(&listener);
        prepare_stream(&stream).unwrap();

        let mut data = String::new();
        let err = read_message(&stream, &mut data).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData, "{err:?}");
        assert!(data.is_empty());

        release_tx.send(()).unwrap();
        client.join().unwrap();
        let _ = fs::remove_file(&path);
    }
}
