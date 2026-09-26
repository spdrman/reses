//! `UreqTransport` against a tiny HTTP server on 127.0.0.1, for the limits and the body
//! timeout. Nothing leaves the machine (or the container).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use reses::s3::{BodyLimit, ERROR_BODY_LIMIT, HttpRequest, Transport, UreqTransport};

/// Serve one connection: read the request head, then run `reply` on the socket.
fn serve_once(reply: impl FnOnce(&mut std::net::TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if sock.read(&mut byte).unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
        reply(&mut sock);
    });
    format!("http://{addr}/bucket/key")
}

fn respond(status: &str, body: Vec<u8>) -> impl FnOnce(&mut std::net::TcpStream) + Send {
    move |sock| {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(&body);
    }
}

fn get(url: String, limit: BodyLimit) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url,
        body_limit: limit,
        ..HttpRequest::default()
    }
}

fn quick() -> UreqTransport {
    UreqTransport::with_timeouts(
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_millis(500),
    )
}

#[test]
fn a_body_within_the_limit_comes_back_whole() {
    let url = serve_once(respond("200 OK", vec![7u8; 100]));
    let resp = quick()
        .send(&get(
            url,
            BodyLimit {
                ok: 100,
                partial: 1,
            },
        ))
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, vec![7u8; 100]);
    assert!(!resp.truncated);
}

#[test]
fn a_body_over_the_limit_stops_at_the_limit() {
    let url = serve_once(respond("200 OK", vec![7u8; 5000]));
    let resp = quick()
        .send(&get(
            url,
            BodyLimit {
                ok: 100,
                partial: 1,
            },
        ))
        .unwrap();
    assert_eq!(resp.body.len(), 100);
    assert!(resp.truncated);
}

#[test]
fn a_206_uses_the_partial_limit() {
    let url = serve_once(respond("206 Partial Content", vec![1u8; 50]));
    let resp = quick()
        .send(&get(url, BodyLimit { ok: 1, partial: 20 }))
        .unwrap();
    assert_eq!(resp.status, 206);
    assert_eq!(resp.body.len(), 20);
    assert!(resp.truncated);
}

#[test]
fn an_error_body_is_capped() {
    let big = vec![b'x'; (ERROR_BODY_LIMIT + 10) as usize];
    let url = serve_once(respond("404 Not Found", big));
    let resp = quick()
        .send(&get(
            url,
            BodyLimit {
                ok: u64::MAX,
                partial: u64::MAX,
            },
        ))
        .unwrap();
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body.len() as u64, ERROR_BODY_LIMIT);
    assert!(resp.truncated);
}

#[test]
fn a_body_that_stalls_times_out() {
    let url = serve_once(|sock| {
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nstart");
        let _ = sock.flush();
        thread::sleep(Duration::from_secs(10));
    });
    let started = Instant::now();
    let result = quick().send(&get(
        url,
        BodyLimit {
            ok: 10_000,
            partial: 10_000,
        },
    ));
    let took = started.elapsed();
    assert!(result.is_err(), "{result:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn a_timed_out_body_under_the_limit_is_an_error_not_a_short_body() {
    // The body stops short of its Content-Length and the connection closes: that must be
    // an error, never a silently short object.
    let url = serve_once(|sock| {
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nonly this");
    });
    let result = quick().send(&get(
        url,
        BodyLimit {
            ok: 10_000,
            partial: 10_000,
        },
    ));
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn reading_stops_at_the_limit_without_waiting_for_the_rest() {
    // The server sends a byte past the limit and then stalls. A transport that drained the
    // whole body would sit in the stall until the body timeout and fail.
    let url = serve_once(|sock| {
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n");
        let _ = sock.write_all(&[9u8; 101]);
        let _ = sock.flush();
        thread::sleep(Duration::from_secs(10));
    });
    let started = Instant::now();
    let resp = quick()
        .send(&get(
            url,
            BodyLimit {
                ok: 100,
                partial: 1,
            },
        ))
        .unwrap();
    assert!(resp.truncated);
    assert_eq!(resp.body, vec![9u8; 100]);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
}
