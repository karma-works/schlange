//! A small HTTP/1.1 server for the webhook endpoint, with the limits a public listener needs:
//!
//! - a fixed pool of handler threads fed by one accept thread through a bounded queue; when
//!   the queue is full a connection is answered with 503 at once instead of waiting;
//! - per connection a read timeout on the socket and an overall deadline for headers and body,
//!   so a slow client (slowloris, a trickled body) occupies one handler for at most the
//!   deadline and never stalls webhooks or `/healthz`;
//! - header and body size limits (GitHub caps payloads at 25 MB), `Content-Length` required
//!   for bodies (no chunked uploads), one request per connection (`Connection: close`).
//!
//! It replaces `tiny_http` here, which reads bodies without a timeout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAX_HEADER: usize = 64 * 1024;
pub const MAX_BODY: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Limits {
    pub threads: usize,
    /// Connections waiting for a handler before new ones get 503.
    pub queue: usize,
    /// Socket read timeout (each read).
    pub read_timeout: Duration,
    /// Headers and body must arrive within this.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            threads: 4,
            queue: 64,
            read_timeout: Duration::from_secs(5),
            deadline: Duration::from_secs(15),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Path with query.
    pub url: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
        }
    }

    pub fn json(status: u16, body: String) -> Self {
        Response {
            status,
            content_type: "application/json",
            body: body.into_bytes(),
        }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

pub struct Server {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn write_response(stream: &mut TcpStream, r: &Response) {
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
    let _ = stream.flush();
}

enum ReadError {
    Timeout,
    TooLarge(u16),
    Bad(&'static str),
    LengthRequired,
    Closed,
}

fn read_request(stream: &mut TcpStream, limits: &Limits) -> Result<Request, ReadError> {
    let deadline = Instant::now() + limits.deadline;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADER {
            return Err(ReadError::TooLarge(431));
        }
        if Instant::now() > deadline {
            return Err(ReadError::Timeout);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReadError::Timeout);
            }
            Err(_) => return Err(ReadError::Closed),
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| ReadError::Bad("headers"))?;
    let mut lines = head.split("\r\n");
    let start = lines.next().ok_or(ReadError::Bad("request line"))?;
    let mut parts = start.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("").to_string();
    if method.is_empty() || !url.starts_with('/') {
        return Err(ReadError::Bad("request line"));
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            return Err(ReadError::Bad("header"));
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    if get("transfer-encoding").is_some() {
        return Err(ReadError::LengthRequired);
    }
    let len = match get("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| ReadError::Bad("length"))?,
        None if method == "POST" || method == "PUT" => return Err(ReadError::LengthRequired),
        None => 0,
    };
    if len > MAX_BODY {
        return Err(ReadError::TooLarge(413));
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        if Instant::now() > deadline {
            return Err(ReadError::Timeout);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReadError::Timeout);
            }
            Err(_) => return Err(ReadError::Closed),
        }
    }
    body.truncate(len);
    Ok(Request {
        method,
        url,
        headers,
        body,
    })
}

fn handle_conn(mut stream: TcpStream, limits: &Limits, handler: &Handler) {
    let _ = stream.set_read_timeout(Some(limits.read_timeout));
    let _ = stream.set_write_timeout(Some(limits.read_timeout));
    let resp = match read_request(&mut stream, limits) {
        Ok(req) => handler(&req),
        Err(ReadError::Closed) => return,
        Err(ReadError::Timeout) => Response::text(408, "request timeout"),
        Err(ReadError::TooLarge(s)) => Response::text(s, "too large"),
        Err(ReadError::LengthRequired) => Response::text(411, "content-length required"),
        Err(ReadError::Bad(what)) => Response::text(400, format!("malformed {what}")),
    };
    write_response(&mut stream, &resp);
}

impl Server {
    pub fn start(addr: SocketAddr, limits: Limits, handler: Handler) -> std::io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx): (SyncSender<TcpStream>, Receiver<TcpStream>) =
            sync_channel(limits.queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::new();
        for i in 0..limits.threads.max(1) {
            let (rx, handler, limits) = (rx.clone(), handler.clone(), limits.clone());
            threads.push(
                std::thread::Builder::new()
                    .name(format!("http-{i}"))
                    .spawn(move || {
                        loop {
                            let next = rx.lock().expect("http queue").recv();
                            match next {
                                Ok(s) => handle_conn(s, &limits, &handler),
                                Err(_) => return,
                            }
                        }
                    })?,
            );
        }
        let st = stop.clone();
        threads.push(
            std::thread::Builder::new()
                .name("http-accept".into())
                .spawn(move || {
                    for conn in listener.incoming() {
                        if st.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(s) = conn else { continue };
                        match tx.try_send(s) {
                            Ok(()) => {}
                            Err(TrySendError::Full(mut s)) => {
                                let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
                                write_response(&mut s, &Response::text(503, "busy"));
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                })?,
        );
        Ok(Server {
            addr,
            stop,
            threads,
        })
    }

    /// Stops accepting and joins the threads (handlers finish their current request).
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept thread.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        for t in self.threads {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(addr: SocketAddr, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(raw).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn a_slow_client_does_not_stall_the_others() {
        let limits = Limits {
            threads: 2,
            queue: 8,
            read_timeout: Duration::from_millis(200),
            deadline: Duration::from_millis(600),
        };
        let handler: Handler = Arc::new(|r: &Request| {
            Response::text(200, format!("{} {} {}", r.method, r.url, r.body.len()))
        });
        let srv = Server::start("127.0.0.1:0".parse().unwrap(), limits, handler).unwrap();
        let addr = srv.addr;
        // Two slow clients: one sends half a header, one promises a body it never sends.
        let mut slow1 = TcpStream::connect(addr).unwrap();
        slow1.write_all(b"POST /webhook HTTP/1.1\r\nx-a: ").unwrap();
        let mut slow2 = TcpStream::connect(addr).unwrap();
        slow2
            .write_all(b"POST /webhook HTTP/1.1\r\ncontent-length: 100\r\n\r\nabc")
            .unwrap();
        let t0 = Instant::now();
        let r = get(addr, b"GET /healthz HTTP/1.1\r\nhost: x\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.ends_with("GET /healthz 0"), "{r}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        // The slow ones are cut off with 408.
        let mut out = String::new();
        slow2
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let _ = slow2.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 408"), "{out}");
        let r = get(
            addr,
            b"POST /webhook HTTP/1.1\r\ncontent-length: 3\r\n\r\nxyz",
        );
        assert!(r.ends_with("POST /webhook 3"), "{r}");
        let r = get(
            addr,
            b"POST /webhook HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n",
        );
        assert!(r.starts_with("HTTP/1.1 411"), "{r}");
        let r = get(
            addr,
            format!(
                "POST /webhook HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
                MAX_BODY + 1
            )
            .as_bytes(),
        );
        assert!(r.starts_with("HTTP/1.1 413"), "{r}");
        drop(slow1);
        srv.shutdown();
    }
}
