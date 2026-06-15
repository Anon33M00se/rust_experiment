//! Toy OpenAI-compatible server.
//!
//! Dependency-free (std only) so it compiles to a tiny, fully static musl
//! binary. It answers the standard OpenAI endpoints with pre-canned responses
//! — there is no model behind it, every reply is hard-coded.
//!
//! Endpoints:
//!   GET  /health                 -> liveness/readiness probe
//!   GET  /v1/models              -> model list
//!   POST /v1/chat/completions    -> chat (supports `"stream": true` via SSE)
//!   POST /v1/completions         -> legacy text completion
//!   POST /v1/embeddings          -> embedding vector
//!
//! Listens on 0.0.0.0:$PORT (default 8000), thread-per-connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const MODEL: &str = "toy-openai";

fn main() {
    // Bind the port nginx proxies to (config.yaml `docker_server.server_port`).
    // NB: we deliberately do NOT read the generic `PORT` env — Baseten injects
    // PORT=8080 into the container, but in docker_server mode 8080 is owned by
    // nginx. Use a dedicated, collision-free override instead.
    let port = std::env::var("TOY_SERVER_PORT").unwrap_or_else(|_| "8000".to_string());
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });
    println!("toy-openai-server listening on {addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn handle(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Request line: e.g. "POST /v1/chat/completions HTTP/1.1"
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();
    let path = raw_path.split('?').next().unwrap_or("/"); // strip query string

    // Headers — we only care about Content-Length.
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(value) = trimmed.split(':').nth(1).map(str::trim) {
            if trimmed.to_ascii_lowercase().starts_with("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
        }
    }

    // Body (only for POSTs).
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    route(&mut writer, &method, path, &body)
}

fn route(w: &mut TcpStream, method: &str, path: &str, body: &str) -> std::io::Result<()> {
    match (method, path) {
        ("GET", "/health") | ("GET", "/") => {
            send_json(w, 200, r#"{"status":"ok"}"#)
        }
        ("GET", "/v1/models") => send_json(w, 200, &models_body()),
        ("POST", "/v1/chat/completions") => {
            if wants_stream(body) {
                send_chat_stream(w)
            } else {
                send_json(w, 200, &chat_body())
            }
        }
        ("POST", "/v1/completions") => send_json(w, 200, &completions_body()),
        ("POST", "/v1/embeddings") => send_json(w, 200, &embeddings_body()),
        _ => send_json(
            w,
            404,
            r#"{"error":{"message":"unknown route (toy server)","type":"not_found"}}"#,
        ),
    }
}

/// Naive detection of `"stream": true` in the request body — good enough for a
/// canned toy server (no JSON parser pulled in).
fn wants_stream(body: &str) -> bool {
    body.replace([' ', '\n', '\t'], "").contains("\"stream\":true")
}

fn send_json(w: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    w.write_all(resp.as_bytes())?;
    w.flush()
}

/// Server-Sent Events stream of chat completion chunks, OpenAI-style.
fn send_chat_stream(w: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\r\n";
    w.write_all(headers.as_bytes())?;

    let created = now();
    let id = "chatcmpl-toystream0";
    let role_chunk = format!(
        r#"{{"id":"{id}","object":"chat.completion.chunk","created":{created},"model":"{MODEL}","choices":[{{"index":0,"delta":{{"role":"assistant"}},"finish_reason":null}}]}}"#
    );
    write_sse(w, &role_chunk)?;

    for token in ["Hello ", "from ", "the ", "toy ", "Rust ", "server!"] {
        let chunk = format!(
            r#"{{"id":"{id}","object":"chat.completion.chunk","created":{created},"model":"{MODEL}","choices":[{{"index":0,"delta":{{"content":"{token}"}},"finish_reason":null}}]}}"#
        );
        write_sse(w, &chunk)?;
    }

    let stop_chunk = format!(
        r#"{{"id":"{id}","object":"chat.completion.chunk","created":{created},"model":"{MODEL}","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}]}}"#
    );
    write_sse(w, &stop_chunk)?;
    w.write_all(b"data: [DONE]\n\n")?;
    w.flush()
}

fn write_sse(w: &mut TcpStream, data: &str) -> std::io::Result<()> {
    w.write_all(format!("data: {data}\n\n").as_bytes())
}

fn models_body() -> String {
    let created = now();
    format!(
        r#"{{"object":"list","data":[{{"id":"{MODEL}","object":"model","created":{created},"owned_by":"rust-experiment"}}]}}"#
    )
}

fn chat_body() -> String {
    let created = now();
    format!(
        r#"{{"id":"chatcmpl-toy0","object":"chat.completion","created":{created},"model":"{MODEL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"Hello from the toy Rust OpenAI server! This is a canned response."}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":9,"completion_tokens":13,"total_tokens":22}}}}"#
    )
}

fn completions_body() -> String {
    let created = now();
    format!(
        r#"{{"id":"cmpl-toy0","object":"text_completion","created":{created},"model":"{MODEL}","choices":[{{"text":"Hello from the toy Rust OpenAI server! This is a canned response.","index":0,"logprobs":null,"finish_reason":"stop"}}],"usage":{{"prompt_tokens":5,"completion_tokens":13,"total_tokens":18}}}}"#
    )
}

fn embeddings_body() -> String {
    // A fixed, tiny canned embedding vector.
    format!(
        r#"{{"object":"list","data":[{{"object":"embedding","index":0,"embedding":[0.0011,-0.0022,0.0033,-0.0044,0.0055,-0.0066,0.0077,-0.0088]}}],"model":"{MODEL}","usage":{{"prompt_tokens":5,"total_tokens":5}}}}"#
    )
}
