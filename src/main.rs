//! status-api: answers `GET /status.json` with the machine's current state.
//!
//! A deliberately tiny HTTP/1.1 responder over std: one request per
//! connection (`Connection: close`), a thread per connection, and a small
//! concurrency cap. It is meant to sit behind a reverse proxy on a Unix
//! socket handed over by systemd socket activation.

mod status;

use serde_json::json;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_HEAD: usize = 8 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT: usize = 8;
const SD_LISTEN_FDS_START: RawFd = 3;

const USAGE: &str = "\
usage: status-api [--listen ADDR] [--disk PATH]...

  --listen ADDR  /path → Unix socket, otherwise TCP host:port.
                 Not needed under systemd socket activation (fd 3).
  --disk PATH    report usage of the filesystem holding PATH (repeatable;
                 default: /)
";

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn set_timeouts(&self) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => {
                s.set_read_timeout(Some(READ_TIMEOUT))?;
                s.set_write_timeout(Some(WRITE_TIMEOUT))
            }
            Conn::Unix(s) => {
                s.set_read_timeout(Some(READ_TIMEOUT))?;
                s.set_write_timeout(Some(WRITE_TIMEOUT))
            }
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.flush(),
            Conn::Unix(s) => s.flush(),
        }
    }
}

impl Listener {
    fn accept(&self) -> io::Result<Conn> {
        match self {
            Listener::Tcp(l) => l.accept().map(|(s, _)| Conn::Tcp(s)),
            Listener::Unix(l) => l.accept().map(|(s, _)| Conn::Unix(s)),
        }
    }
}

/// The socket systemd passed us, if any (sd_listen_fds semantics: only
/// honored when LISTEN_PID is our pid; the variables are then cleared so
/// children such as systemctl don't inherit them).
fn systemd_listener() -> io::Result<Option<Listener>> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    let nfds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);
    // Single-threaded here, so mutating the environment is safe.
    std::env::remove_var("LISTEN_PID");
    std::env::remove_var("LISTEN_FDS");
    std::env::remove_var("LISTEN_FDNAMES");
    if !pid_ok || nfds < 1 {
        return Ok(None);
    }
    if nfds > 1 {
        eprintln!("status-api: {nfds} sockets passed, using only fd 3");
    }
    let fd = SD_LISTEN_FDS_START;
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getsockname(fd, &mut addr as *mut _ as *mut libc::sockaddr, &mut len) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(match addr.ss_family as libc::c_int {
            libc::AF_UNIX => Listener::Unix(UnixListener::from_raw_fd(fd)),
            libc::AF_INET | libc::AF_INET6 => Listener::Tcp(TcpListener::from_raw_fd(fd)),
            f => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("fd 3 has unsupported address family {f}"),
                ))
            }
        }))
    }
}

fn bind(addr: &str) -> io::Result<Listener> {
    if addr.starts_with('/') {
        // A stale socket from a previous run would make bind fail.
        match std::fs::remove_file(addr) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        UnixListener::bind(addr).map(Listener::Unix)
    } else {
        TcpListener::bind(addr).map(Listener::Tcp)
    }
}

fn respond(conn: &mut Conn, code: u16, reason: &str, extra: &str, body: &[u8], head_only: bool) {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         {extra}\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    if !head_only {
        out.extend_from_slice(body);
    }
    let _ = conn.write_all(&out).and_then(|_| conn.flush());
}

fn error(conn: &mut Conn, code: u16, reason: &str, extra: &str, head_only: bool) {
    let body = json!({ "error": reason }).to_string();
    respond(conn, code, reason, extra, body.as_bytes(), head_only);
}

/// Reads up to the end of the request head. Ok(None) means the head
/// exceeded MAX_HEAD.
fn read_head(conn: &mut Conn) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(buf));
        }
        if buf.len() >= MAX_HEAD {
            return Ok(None);
        }
        let want = chunk.len().min(MAX_HEAD - buf.len());
        let n = conn.read(&mut chunk[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn handle(mut conn: Conn, disks: &[String]) {
    if conn.set_timeouts().is_err() {
        return;
    }
    let head = match read_head(&mut conn) {
        Ok(Some(h)) => h,
        Ok(None) => return error(&mut conn, 431, "Request Header Fields Too Large", "", false),
        // Timeout or client gone: nobody to answer.
        Err(_) => return,
    };
    let head = String::from_utf8_lossy(&head);
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return error(&mut conn, 400, "Bad Request", "", false);
    };
    if !version.starts_with("HTTP/1.") {
        return error(&mut conn, 400, "Bad Request", "", false);
    }
    let head_only = method == "HEAD";
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path != "/" && path != "/status.json" {
        return error(&mut conn, 404, "Not Found", "", head_only);
    }
    if method != "GET" && method != "HEAD" {
        return error(&mut conn, 405, "Method Not Allowed", "Allow: GET, HEAD\r\n", false);
    }
    let body = status::collect(disks).to_string();
    respond(&mut conn, 200, "OK", "", body.as_bytes(), head_only);
}

/// Decrements the in-flight counter when the connection thread ends,
/// even if it panics.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn main() {
    let mut listen: Option<String> = None;
    let mut disks: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next(),
            "--disk" => match args.next() {
                Some(d) => disks.push(d),
                None => die("--disk needs a PATH"),
            },
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => die(&format!("unknown argument {other:?}")),
        }
    }
    if disks.is_empty() {
        disks.push("/".to_owned());
    }

    let listener = match systemd_listener() {
        Ok(Some(l)) => l,
        Ok(None) => match &listen {
            Some(addr) => bind(addr).unwrap_or_else(|e| die(&format!("bind {addr}: {e}"))),
            None => die("no socket from systemd and no --listen given"),
        },
        Err(e) => die(&format!("socket activation: {e}")),
    };

    let disks: Arc<[String]> = disks.into();
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let mut conn = match listener.accept() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("accept: {e}");
                // Avoid a hot loop on e.g. EMFILE.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT {
            active.fetch_sub(1, Ordering::SeqCst);
            // Answered inline without reading the request; the write
            // timeout bounds how long a stuck client can hold this loop.
            let _ = conn.set_timeouts();
            error(&mut conn, 503, "Service Unavailable", "Retry-After: 1\r\n", false);
            continue;
        }
        let slot = Slot(active.clone());
        let disks = disks.clone();
        let spawned = thread::Builder::new().spawn(move || {
            let _slot = slot;
            handle(conn, &disks);
        });
        if let Err(e) = spawned {
            eprintln!("spawn: {e}");
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("status-api: {msg}\n\n{USAGE}");
    std::process::exit(2);
}
