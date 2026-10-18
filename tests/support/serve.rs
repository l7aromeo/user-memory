//! One tool call to `user-memory serve`, for tools with no command of their
//! own, such as link.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

/// The reply of tool `name` called with `arguments`, parsed as JSON or else
/// as a string, from the server `serve` starts: a `user-memory serve`
/// command with its folder and environment set.
pub fn call(mut serve: Command, name: &str, arguments: Value) -> Value {
    let mut child = serve
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    input.flush().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let reply = loop {
        let mut line = String::new();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "serve closed before answering {name}"
        );
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["id"] == 2 {
            break message;
        }
    };
    drop(input);
    child.wait().unwrap();
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: {reply}"));
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}
