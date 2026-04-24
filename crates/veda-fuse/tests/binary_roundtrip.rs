//! Integration test: verify VedaClient::read_file returns raw bytes untouched,
//! including invalid-UTF-8 sequences typical of binary files.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use veda_fuse::client::VedaClient;

/// Spawn a one-shot HTTP server that returns the given body for the next GET.
/// Returns the base URL (e.g. "http://127.0.0.1:PORT").
fn spawn_mock(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Read request headers until we see the CRLFCRLF terminator.
            // We don't need the body for this mock (GET has no body).
            let mut header_buf: Vec<u8> = Vec::with_capacity(1024);
            let mut chunk = [0u8; 512];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        header_buf.extend_from_slice(&chunk[..n]);
                        if header_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn read_file_round_trips_non_utf8_bytes() {
    let binary: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x80, 0x81, 0xC0, 0xC1, 0xF5, 0xFF, 0x00];
    let base = spawn_mock(binary.clone());
    let client = VedaClient::new(&base, "dummy-key");
    let got = client.read_file("/blob").expect("read_file should not error on binary");
    assert_eq!(got, binary, "bytes must round-trip unchanged");
}

#[test]
fn read_file_round_trips_empty_body() {
    let base = spawn_mock(Vec::new());
    let client = VedaClient::new(&base, "dummy-key");
    let got = client.read_file("/empty").expect("empty body is valid");
    assert!(got.is_empty());
}
