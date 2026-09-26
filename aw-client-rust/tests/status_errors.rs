use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;

use aw_client_rust::blocking;
use aw_client_rust::AwClient;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(future)
}

struct MockResponse {
    status_line: &'static str,
    content_type: &'static str,
    body: &'static str,
}

/// Drain the HTTP request fully before responding, returning its request line.
///
/// Parses Content-Length from headers so POST body data (which may arrive
/// in a separate TCP segment) is consumed before the mock writes its
/// response. Without this, reqwest may see a broken pipe on loopback if
/// the response arrives before the body finishes sending.
fn drain_request(stream: &mut impl Read) -> String {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0_usize;
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read request line");
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read request line");
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of headers; body follows if Content-Length > 0
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body_buf = vec![0_u8; content_length];
        reader
            .read_exact(&mut body_buf)
            .expect("drain request body");
    }
    request_line.trim().to_string()
}

/// The join handle yields the request line of every request the mock served.
fn spawn_mock_server(responses: Vec<MockResponse>) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
    let port = listener.local_addr().expect("mock server addr").port();
    let handle = thread::spawn(move || {
        let mut request_lines = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept request");
            request_lines.push(drain_request(&mut stream));
            let body = response.body.as_bytes();
            write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n{}",
                response.status_line,
                body.len(),
                response.content_type,
                response.body
            )
            .expect("write response");
            stream.flush().expect("flush response");
        }
        request_lines
    });
    (port, handle)
}

#[test]
fn async_client_rejects_non_success_statuses() {
    let (port, handle) = spawn_mock_server(vec![
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: "{}",
        },
        MockResponse {
            status_line: "409 Conflict",
            content_type: "text/plain",
            body: "",
        },
    ]);
    let client = AwClient::new("127.0.0.1", port, "aw-client-rust-test").expect("create client");

    let err = block_on(client.get_buckets()).expect_err("500 response must fail");
    assert_eq!(
        err.status(),
        Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    );

    let err = block_on(client.create_bucket_simple("bucket", "type"))
        .expect_err("409 response must fail");
    assert_eq!(err.status(), Some(reqwest::StatusCode::CONFLICT));

    handle.join().expect("join mock server");
}

#[test]
fn blocking_client_rejects_non_success_statuses() {
    let (port, handle) = spawn_mock_server(vec![
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: "{}",
        },
        MockResponse {
            status_line: "409 Conflict",
            content_type: "text/plain",
            body: "",
        },
    ]);
    let client =
        blocking::AwClient::new("127.0.0.1", port, "aw-client-rust-test").expect("create client");

    let err = client
        .get_buckets()
        .expect_err("500 response must fail for blocking client");
    assert_eq!(
        err.status(),
        Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    );

    let err = client
        .create_bucket_simple("bucket", "type")
        .expect_err("409 response must fail for blocking client");
    assert_eq!(err.status(), Some(reqwest::StatusCode::CONFLICT));

    handle.join().expect("join mock server");
}

#[test]
fn get_event_maps_404_to_none_and_rejects_other_errors() {
    let (port, handle) = spawn_mock_server(vec![
        MockResponse {
            status_line: "404 Not Found",
            content_type: "application/json",
            body: r#"{"message":"missing"}"#,
        },
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: "{}",
        },
    ]);
    let client = AwClient::new("127.0.0.1", port, "aw-client-rust-test").expect("create client");

    let event = block_on(client.get_event("bucket", 1)).expect("404 must not be an error");
    assert!(event.is_none());

    let err = block_on(client.get_event("bucket", 2)).expect_err("500 response must fail");
    assert_eq!(
        err.status(),
        Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
    );

    handle.join().expect("join mock server");
}

#[test]
fn bucket_ids_and_setting_keys_are_encoded_as_one_path_segment() {
    let respond = |body: &'static str| MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body,
    };
    let (port, handle) = spawn_mock_server(vec![
        respond("[]"),
        respond("0"),
        respond(""),
        respond(""),
        respond(""),
        respond("null"),
    ]);
    let client = AwClient::new("127.0.0.1", port, "aw-client-rust-test").expect("create client");
    let bucket = "a#b?c/d";
    let event = aw_client_rust::Event {
        id: None,
        timestamp: chrono::Utc::now(),
        duration: chrono::Duration::zero(),
        data: serde_json::Map::new(),
    };

    block_on(client.get_events(bucket, None, None, Some(1))).expect("get events");
    block_on(client.get_event_count(bucket)).expect("count events");
    block_on(client.heartbeat(bucket, &event, 5.0)).expect("heartbeat");
    block_on(client.delete_event(bucket, 7)).expect("delete event");
    block_on(client.delete_bucket(bucket)).expect("delete bucket");
    block_on(client.get_setting("ui#theme")).expect("get setting");

    let requests = handle.join().expect("join mock server");
    assert_eq!(
        requests,
        vec![
            "GET /api/0/buckets/a%23b%3Fc%2Fd/events?limit=1 HTTP/1.1",
            "GET /api/0/buckets/a%23b%3Fc%2Fd/events/count HTTP/1.1",
            "POST /api/0/buckets/a%23b%3Fc%2Fd/heartbeat?pulsetime=5 HTTP/1.1",
            "DELETE /api/0/buckets/a%23b%3Fc%2Fd/events/7 HTTP/1.1",
            "DELETE /api/0/buckets/a%23b%3Fc%2Fd HTTP/1.1",
            "GET /api/0/settings/ui%23theme HTTP/1.1",
        ]
    );
}

#[test]
fn non_hierarchical_base_url_is_an_error_not_a_panic() {
    let mut client =
        AwClient::new("127.0.0.1", 5600, "aw-client-rust-test-bad-base").expect("create client");
    client.baseurl = reqwest::Url::parse("data:text/plain,hello").unwrap();

    assert!(block_on(client.get_info()).is_err());
    block_on(client.delete_bucket("bucket")).expect_err("a data: base URL must fail");
}
