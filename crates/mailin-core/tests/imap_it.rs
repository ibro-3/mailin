//! Integration harness against GreenMail (see `docker-compose.it.yml`).
//!
//! These tests are ignored by default because they need the container. The
//! IMAP/SMTP client lands in milestone 2; for now the harness proves that CI
//! can reach a real server and that credentials are configured as expected.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

fn addr(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

#[test]
#[ignore = "requires GreenMail: cargo xtask it-up"]
fn greenmail_speaks_imap() {
    let addr = addr("MAILIN_IT_IMAP_ADDR", "127.0.0.1:3143");
    let mut stream = TcpStream::connect(&addr).expect("connect to GreenMail IMAP");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut greeting = String::new();
    reader.read_line(&mut greeting).expect("greeting");
    assert!(
        greeting.starts_with("* OK"),
        "unexpected greeting: {greeting:?}"
    );

    stream
        .write_all(b"a1 LOGIN ada correct-horse\r\n")
        .expect("login");
    let mut response = String::new();
    loop {
        response.clear();
        reader.read_line(&mut response).expect("read");
        if response.starts_with("a1 ") {
            break;
        }
    }
    assert!(response.starts_with("a1 OK"), "login failed: {response:?}");

    stream.write_all(b"a2 LOGOUT\r\n").expect("logout");
}

#[test]
#[ignore = "requires GreenMail: cargo xtask it-up"]
fn greenmail_speaks_smtp() {
    let addr = addr("MAILIN_IT_SMTP_ADDR", "127.0.0.1:3025");
    let mut stream = TcpStream::connect(&addr).expect("connect to GreenMail SMTP");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("greeting");
    assert!(line.starts_with("220"), "unexpected greeting: {line:?}");

    stream.write_all(b"EHLO mailin.test\r\n").expect("ehlo");
    loop {
        line.clear();
        reader.read_line(&mut line).expect("read");
        if line.starts_with("250 ") {
            break;
        }
    }
    stream.write_all(b"QUIT\r\n").expect("quit");
}
