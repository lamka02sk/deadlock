//! Single instance: the first process listens on a local socket, later launches forward
//! their request to it and exit.
//!
//! The protocol is one line per connection: an absolute project path, or an empty line
//! for "show the open-project window".

use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a forwarded launch asks the running process to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Open or focus the project at this canonical path.
    Open(PathBuf),
    /// Show the open-project window.
    Welcome,
}

/// The result of [`claim`].
#[derive(Debug)]
pub enum Instance {
    /// This process is the first: serve requests from `listener`.
    Primary(UnixListener),
    /// Another process is running and has the request.
    Forwarded,
}

/// `deadlock.sock` in the user's runtime directory. Without one, the temp directory,
/// with the user name in the file name because `/tmp` is shared.
pub fn socket_path() -> PathBuf {
    match dirs::runtime_dir() {
        Some(dir) => dir.join("deadlock.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_default();
            std::env::temp_dir().join(format!("deadlock-{user}.sock"))
        }
    }
}

/// Becomes the primary instance, or forwards `request` to the running one.
///
/// A socket file nobody listens on is left by a crash; it is removed and taken over.
///
/// # Errors
/// Fails when the socket can be neither reached nor created.
pub fn claim(socket: &Path, request: &Request) -> io::Result<Instance> {
    let mut last = None;
    // Two rounds: a process starting at the same moment may bind between our steps.
    for _ in 0..2 {
        match UnixStream::connect(socket) {
            Ok(mut stream) => {
                let line = match request {
                    Request::Open(path) => path.to_str().ok_or_else(|| {
                        io::Error::new(ErrorKind::InvalidInput, "project path is not UTF-8")
                    })?,
                    Request::Welcome => "",
                };
                stream.write_all(line.as_bytes())?;
                stream.write_all(b"\n")?;
                return Ok(Instance::Forwarded);
            }
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                if let Err(e) = std::fs::remove_file(socket)
                    && e.kind() != ErrorKind::NotFound
                {
                    return Err(e);
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match UnixListener::bind(socket) {
            Ok(listener) => return Ok(Instance::Primary(listener)),
            Err(e) if e.kind() == ErrorKind::AddrInUse => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| ErrorKind::AddrInUse.into()))
}

/// Accepts connections forever, calling `on_request` for each well-formed one.
/// Blocks: run it on its own thread.
pub fn serve(listener: &UnixListener, mut on_request: impl FnMut(Request)) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // A client that never sends its line must not stall the next launch. macOS
        // refuses the timeout (EINVAL) once the client has closed its end, which is the
        // usual case; the read then ends at the end of the stream, so the error is ignored.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line).is_ok() {
            let line = line.trim_end_matches('\n');
            on_request(if line.is_empty() {
                Request::Welcome
            } else {
                Request::Open(PathBuf::from(line))
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_claim_forwards_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let Instance::Primary(listener) = claim(&socket, &Request::Welcome).unwrap() else {
            panic!("first claim should be primary");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || serve(&listener, |r| tx.send(r).unwrap()));

        let path = PathBuf::from("/some/project");
        assert!(matches!(
            claim(&socket, &Request::Open(path.clone())).unwrap(),
            Instance::Forwarded
        ));
        assert!(matches!(
            claim(&socket, &Request::Welcome).unwrap(),
            Instance::Forwarded
        ));
        let timeout = Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(timeout).unwrap(), Request::Open(path));
        assert_eq!(rx.recv_timeout(timeout).unwrap(), Request::Welcome);
    }

    #[test]
    fn stale_socket_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        // A crashed primary: the file stays, nobody listens.
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        assert!(matches!(
            claim(&socket, &Request::Welcome).unwrap(),
            Instance::Primary(_)
        ));
    }
}
