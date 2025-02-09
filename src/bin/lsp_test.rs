use anyhow::Result;
use serde_json::Value;
use simple_completion_language_server::{server, StartOptions};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;

struct AsyncIn(UnboundedReceiver<String>);
struct AsyncOut(UnboundedSender<String>);

// Server reads request using AsyncIn
impl AsyncRead for AsyncIn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let rx = self.get_mut();
        match rx.0.poll_recv(cx) {
            Poll::Ready(Some(v)) => {
                buf.put_slice(v.as_bytes());
                Poll::Ready(Ok(()))
            }
            _ => Poll::Pending,
        }
    }
}

// Server writes response using AsyncOut
impl AsyncWrite for AsyncOut {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let tx = self.get_mut();
        if let Ok(value) = String::from_utf8(buf.to_vec()) {
            let _ = tx.0.send(value);
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn encode_lsp_message(msg: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg)
}

fn handle_server_message(response: &str) -> Result<bool> {
    // Find start of JSON content
    let json_str = &response[response.find('{').unwrap_or(0)..];
    let json: Value = serde_json::from_str(json_str)?;

    // Check if it's a log message
    if json.get("method") == Some(&Value::String("window/logMessage".to_string())) {
        if let Some(params) = json.get("params") {
            // Extract type and message
            let type_num = params.get("type").and_then(|v| v.as_u64()).unwrap_or(4);
            let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");

            // Write to stderr based on message type
            match type_num {
                1 => eprintln!("ERROR: {}", message),
                2 => eprintln!("WARN:  {}", message),
                3 => eprintln!("INFO:  {}", message),
                4 => eprintln!("DEBUG: {}", message),
                _ => eprintln!("LOG:   {}", message),
            }
            return Ok(true);
        }
    }

    // For non-log messages, print as before
    println!("Server message: {}", serde_json::to_string_pretty(&json)?);
    Ok(false)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup paths
    let home = std::env::var("HOME").unwrap_or_default();
    let config_dir = PathBuf::from(&home).join(".config/helix");

    let start_options = StartOptions {
        home_dir: home,
        snippets_path: config_dir.join("snippets"),
        external_snippets_config_path: config_dir.join("external-snippets.toml"),
        unicode_input_path: config_dir.join("unicode-input"),
    };

    // Load snippets
    let snippets = simple_completion_language_server::snippets::config::load_snippets(&start_options)?;
    eprintln!("INFO:  Loaded {} snippets", snippets.len());

    // 1. User input -> Channel: request_tx
    let (request_tx, rx) = mpsc::unbounded_channel::<String>();
    // 2. Channel -> User output: response_rx
    let (tx, mut response_rx) = mpsc::unbounded_channel::<String>();

    // Start server
    let server_handle = tokio::spawn(server::start(
        AsyncIn(rx),
        AsyncOut(tx),
        snippets,
        HashMap::new(), // No unicode mappings
        start_options.home_dir,
    ));

    // 1. Initialize server with capabilities request
    let init_msg = r#"{"jsonrpc":"2.0","method":"initialize","params":{"capabilities":{}},"id":1}"#;
    request_tx.send(encode_lsp_message(init_msg))?;

    // 2. Configure server features
    let config_msg = r#"{"jsonrpc":"2.0","method":"workspace/didChangeConfiguration","params":{"settings":{"feature_snippets":true,"snippets_first":true,"feature_words":true}},"id":2}"#;
    request_tx.send(encode_lsp_message(config_msg))?;

    // 3. Wait for responses: Handle initialization responses
    for _ in 0..2 {  // Waits for 2 responses (init + config)
        if let Some(response) = response_rx.recv().await {
            if let Err(e) = handle_server_message(&response) {
                eprintln!("ERROR: Failed to handle server message: {}", e);
            }
        }
    }

    // Interactive mode
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut buffer = String::new();

    eprintln!("\nINFO:  Example commands:");
    eprintln!("INFO:  1. Open document with snippet trigger:");
    eprintln!(
        r#"INFO:  {{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"languageId":"python","text":"ma","uri":"file:///tmp/test.py","version":0}}}}}}"#
    );
    eprintln!("\nINFO:  2. Request completion:");
    eprintln!(
        r#"INFO:  {{"jsonrpc":"2.0","method":"textDocument/completion","params":{{"position":{{"character":2,"line":0}},"textDocument":{{"uri":"file:///tmp/test.py"}}}},"id":3}}"#
    );

    loop {
        print!("\n> ");
        stdout.flush()?;
        buffer.clear();

        if stdin.lock().read_line(&mut buffer)? == 0 {
            break;
        }

        let input = buffer.trim();
        if input.is_empty() {
            continue;
        }

        // Parse JSON to check if it's a notification or request
        match serde_json::from_str::<Value>(input) {
            Ok(json) => {
                request_tx.send(encode_lsp_message(input))?;

                // If it has an id, it's a request and we should wait for response
                if json.get("id").is_some() {
                    match timeout(Duration::from_secs(1), response_rx.recv()).await {
                        Ok(Some(response)) => {
                            if let Err(e) = handle_server_message(&response) {
                                eprintln!("ERROR: Failed to handle server response: {}", e);
                            }
                        }
                        Ok(None) => eprintln!("ERROR: Server closed connection"),
                        Err(_) => eprintln!("WARN:  No response received (timeout)"),
                    }
                } else {
                    eprintln!("INFO:  Notification sent (no response expected)");
                }
            }
            Err(e) => eprintln!("ERROR: Invalid JSON: {}", e),
        }
    }

    // Cleanup
    drop(request_tx);
    server_handle.abort();

    Ok(())
}
