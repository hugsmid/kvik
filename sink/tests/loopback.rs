//! The real HTTP transport against the fake bucket served by an in-process
//! HTTP/1.1 server on the loopback interface: headers and signatures as they
//! go over the wire, `HEAD` replies without a body, and timeouts. No request
//! leaves the machine.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use kvik_sink::fake::{FakeBucket, FakeOp};
use kvik_sink::transport::{Method, Request, Transport};
use kvik_sink::verify::{self, sha256};
use kvik_sink::{RetryPolicy, S3Sink, block_on};
use kvik_sink_api::{BackupSink, SinkError};

/// What the server does with a connection.
#[derive(Clone, Copy)]
enum Mode {
    Serve,
    /// Serve, sending `GET` bodies chunked, with no `Content-Length`.
    Chunked,
    /// Read the request, then never answer.
    Silent,
}

/// Serves `bucket` on 127.0.0.1 until the process ends; returns the port.
fn serve(bucket: FakeBucket, mode: Mode) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let bucket = bucket.clone();
            std::thread::spawn(move || {
                let _ = connection(stream, &bucket, mode);
            });
        }
    });
    port
}

fn connection(stream: TcpStream, bucket: &FakeBucket, mode: Mode) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return Ok(());
        };
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            reader.read_line(&mut h)?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            let (k, v) = h.split_once(':').unwrap();
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
        let len: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0; len];
        reader.read_exact(&mut body)?;
        if let Mode::Silent = mode {
            std::thread::sleep(Duration::from_secs(30));
            return Ok(());
        }
        let method = match method {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "PUT" => Method::Put,
            _ => return Ok(()),
        };
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let query = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        let request = request(method, path, query, headers, body);
        let response = bucket.send(&request).unwrap();
        let chunked = matches!(mode, Mode::Chunked) && method == Method::Get;
        let mut head = format!("HTTP/1.1 {} X\r\n", response.status);
        let mut has_length = false;
        for (k, v) in &response.headers {
            if chunked && k == "content-length" {
                continue;
            }
            has_length |= k == "content-length";
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if chunked {
            head.push_str("transfer-encoding: chunked\r\n");
        } else if !has_length {
            head.push_str(&format!("content-length: {}\r\n", response.body.len()));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if chunked {
            for part in response.body.chunks(3) {
                stream.write_all(format!("{:x}\r\n", part.len()).as_bytes())?;
                stream.write_all(part)?;
                stream.write_all(b"\r\n")?;
            }
            stream.write_all(b"0\r\n\r\n")?;
        } else if method != Method::Head {
            stream.write_all(&response.body)?;
        }
        stream.flush()?;
    }
}

fn request(
    method: Method,
    path: &str,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Request {
    let body = (method == Method::Put).then(|| Arc::from(body));
    Request::new(method, "http://loopback", path, query, headers, body)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn sink_for(bucket: &FakeBucket, port: u16, retry: RetryPolicy, timeout: Duration) -> S3Sink {
    let mut config = bucket
        .config()
        .with_retry(retry)
        .with_timeouts(Duration::from_secs(5), timeout);
    config.endpoint = format!("http://127.0.0.1:{port}");
    S3Sink::new(config).unwrap()
}

#[test]
fn the_http_transport_speaks_the_subset() {
    let bucket = FakeBucket::new();
    let port = serve(bucket.clone(), Mode::Serve);
    let sink = sink_for(
        &bucket,
        port,
        RetryPolicy::default(),
        Duration::from_secs(30),
    );
    let data: Arc<[u8]> = (0..=255u8).cycle().take(100_000).collect::<Vec<_>>().into();
    let copy = block_on(verify::upload(&sink, "log/a b+c", data.clone())).unwrap();
    assert_eq!(copy.stored().size, 100_000);
    assert!(matches!(
        block_on(sink.put("log/a b+c", Arc::from(&b"x"[..]), sha256(b"x"))),
        Err(SinkError::Exists { .. })
    ));
    assert_eq!(
        block_on(verify::download(&sink, "log/a b+c", sha256(&data))).unwrap(),
        &data[..]
    );
    assert_eq!(
        block_on(sink.get("log/a b+c", Some(1000..1010))).unwrap(),
        &data[1000..1010]
    );
    assert_eq!(
        block_on(sink.list("log/", None)).unwrap().keys,
        vec!["log/a b+c"]
    );
    assert_eq!(block_on(sink.stat("log/none")).unwrap(), None);
    assert_eq!(bucket.count(FakeOp::Put), 2);
}

#[test]
fn a_silent_server_times_out_as_a_transient_fault() {
    let bucket = FakeBucket::new();
    let port = serve(bucket.clone(), Mode::Silent);
    let sink = sink_for(
        &bucket,
        port,
        RetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(1)),
        Duration::from_millis(300),
    );
    let started = std::time::Instant::now();
    let r = block_on(sink.stat("log/x"));
    assert!(matches!(r, Err(SinkError::Transient { .. })), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

fn capped_sink(bucket: &FakeBucket, port: u16, max_object_size: u64) -> S3Sink {
    let mut config = bucket.config().with_max_object_size(max_object_size);
    config.endpoint = format!("http://127.0.0.1:{port}");
    S3Sink::new(config).unwrap()
}

#[test]
fn the_http_transport_holds_object_bodies_to_max_object_size() {
    let bucket = FakeBucket::new();
    let sized = serve(bucket.clone(), Mode::Serve);
    let chunked = serve(bucket.clone(), Mode::Chunked);
    let sink = capped_sink(&bucket, sized, 1 << 20);
    for (key, b) in [("log/eight", &b"01234567"[..]), ("log/ten", b"0123456789")] {
        block_on(sink.put(key, Arc::from(b), sha256(b))).unwrap();
    }
    // A declared length is refused before the body is read; a chunked body when it passes the cap.
    for (port, size) in [(sized, 10), (chunked, 9)] {
        let small = capped_sink(&bucket, port, 8);
        assert_eq!(
            block_on(small.get("log/ten", None)),
            Err(SinkError::TooLarge { size, limit: 8 }),
            "port {port}"
        );
        assert_eq!(block_on(small.get("log/eight", None)).unwrap(), b"01234567");
        assert_eq!(
            block_on(small.get("log/none", None)),
            Err(SinkError::NotFound {
                key: "log/none".into()
            })
        );
    }
}
