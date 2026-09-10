use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

#[test]
fn completed_task_does_not_report_usage_or_prompts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for connection in listener.incoming() {
            let mut connection = connection.unwrap();
            let mut reader = BufReader::new(&mut connection);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            if request.starts_with("GET /finished ") {
                break;
            }
            requests.push(request);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            let _ = connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
        requests
    });
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_browser-use-terminal"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "run-fake",
            "private task fixture",
        ])
        .current_dir(temporary.path())
        .env("BROWSER_USE_TERMINAL_HOME", temporary.path().join("config"))
        .env("BUT_POSTHOG_HOST", format!("http://{address}"))
        .env("BUT_POSTHOG_KEY", "test-project")
        .env("BUT_TELEMETRY", "1")
        .env("BUT_PRODUCT_ANALYTICS", "1")
        .env("LLM_BROWSER_BROWSER_MODE", "none")
        .output()
        .unwrap();
    TcpStream::connect(address)
        .unwrap()
        .write_all(b"GET /finished HTTP/1.1\r\n\r\n")
        .unwrap();
    let requests = requests.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session_id = String::from_utf8(output.stdout).unwrap().trim().to_string();
    let store = browser_use_store::Store::open(&state).unwrap();
    let events = store.events_for_session(&session_id).unwrap();
    assert_eq!(
        browser_use_protocol::session_result_from_events(&events).as_deref(),
        Some("Fake result for: private task fixture")
    );
    assert!(
        requests.is_empty(),
        "unexpected reporting requests: {requests:?}"
    );
    assert!(!state.join("product_analytics").exists());
}
