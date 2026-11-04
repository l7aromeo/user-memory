//! A `user-memory serve` kept running across several tool calls, as an
//! agent session keeps it, for tests of what the server does between and
//! after calls. One call on its own is `serve.rs`.

#![allow(dead_code, reason = "each test file uses part of this module")]

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

/// A running `user-memory serve`, initialized.
pub struct Serve {
    pub child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Serve {
    /// Starts `serve`, a `user-memory serve` command with its folder and
    /// environment set, and initializes the session.
    pub fn spawn(mut serve: Command) -> Self {
        let mut child = serve
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut serve = Self {
            child,
            input,
            output,
            next_id: 1,
        };
        serve.ask(
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
        );
        serve.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        serve
    }

    fn send(&mut self, message: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    }

    /// The result of request `method` with `params`.
    pub fn ask(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "serve closed"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message["result"].clone();
            }
        }
    }

    /// The reply of tool `name` called with `arguments`, parsed as JSON.
    pub fn call(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.ask("tools/call", json!({"name":name,"arguments":arguments}));
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {result}"));
        serde_json::from_str(text).unwrap_or_else(|_| panic!("{name}: {text}"))
    }

    /// Ends the session as a client does, closing the server's input, and
    /// returns what the server wrote to stderr.
    pub fn close(mut self) -> String {
        drop(self.input.take());
        let out = crate::time::finish(self.child, "serve");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}
