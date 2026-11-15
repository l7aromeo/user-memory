"""A stand-in for the Anthropic Messages API and the OpenAI Responses API,
so the real Claude Code and Codex can run a turn with no billed model.

It listens on 127.0.0.1 only and forwards nothing: each request is answered
from a script. It keeps each request's path and JSON body, never its
headers, so no token a client sends reaches the record or the log file.

A script maps an API ("anthropic" or "openai") to the replies its main
requests get in order; past the end each gets the text "done". A reply is
`{"text": TEXT}`, `{"tool": NAME, "input": {...}}` or, for OpenAI's
free-text tools such as Codex's `exec`, `{"custom_tool": NAME, "input":
TEXT}`; `{"fail": STATUS}` answers
with that HTTP status and an invalid-request error, as a provider does when
it refuses a request, so a test can see how a client ends a failed turn. A
reply with `"repeat": true` answers every later request too, as a client
that retries a refused request meets the same refusal again. With
`"hold": true` it waits until `release()` before answering, so a test can
act while a turn is under way. Anthropic requests without tools (Claude
Code's own side calls, such as a title) get text and take no reply from the
script.

Usage reported with every reply is fixed (USAGE), so a test can predict a
client's usage figures."""

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# Tokens reported for every reply: input counts cached input, as both APIs'
# clients add them up.
USAGE = {"input": 1000, "cached_input": 900, "output": 50, "reasoning": 20}
# How long a held reply waits for release() before answering anyway, so a
# test that forgets to release cannot hang a client.
HOLD_LIMIT = 60
# The message of a `fail` reply's error, so a test can find it where the
# client reports the failed turn.
FAILURE_MESSAGE = "The fake model refused this request."


class FakeModel:
    def __init__(self, script, log=None):
        self.script = {api: list(replies) for api, replies in script.items()}
        self.log = Path(log) if log else None
        self.requests = []
        self.used = {"anthropic": 0, "openai": 0}
        self.lock = threading.Lock()
        self.released = threading.Event()
        self.held = threading.Event()
        model = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                model.reply_json(self, {"data": [], "models": []})

            def do_POST(self):
                model.answer(self)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self):
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}"

    def start(self):
        self.thread.start()
        return self

    def stop(self):
        self.released.set()
        self.server.shutdown()
        self.server.server_close()

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()

    def release(self):
        """Lets a held reply answer."""
        self.released.set()

    def wait_held(self, timeout=HOLD_LIMIT):
        """Waits until a held reply is waiting; True if one is."""
        return self.held.wait(timeout)

    # ------------------------------------------------------------ requests

    def answer(self, handler):
        raw = handler.rfile.read(int(handler.headers.get("content-length") or 0))
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            body = {"unparsed": raw.decode(errors="replace")}
        api = "openai" if "/responses" in handler.path else "anthropic"
        record = {"api": api, "path": handler.path, "body": body}
        with self.lock:
            self.requests.append(record)
            if self.log:
                with open(self.log, "a") as log:
                    log.write(json.dumps(record) + "\n")
        if "count_tokens" in handler.path:
            return self.reply_json(handler, {"input_tokens": USAGE["input"]})
        reply = self.next_reply(api, body)
        if reply.get("hold"):
            self.held.set()
            self.released.wait(HOLD_LIMIT)
        if "fail" in reply:
            return self.refuse(handler, api, reply["fail"])
        if api == "anthropic":
            self.anthropic(handler, reply, body)
        else:
            self.openai(handler, reply)

    def next_reply(self, api, body):
        with self.lock:
            if api == "anthropic" and not body.get("tools"):
                return {"text": "side"}
            replies = self.script.get(api, [])
            index = self.used[api]
            if index >= len(replies):
                return {"text": "done"}
            if not replies[index].get("repeat"):
                self.used[api] += 1
            return replies[index]

    def main_requests(self, api):
        """Requests that took a reply from the script, in order."""
        return [r for r in self.requests if r["api"] == api
                and (api == "openai" or r["body"].get("tools"))]

    # ------------------------------------------------------------ replies

    @staticmethod
    def reply_json(handler, value):
        data = json.dumps(value).encode()
        handler.send_response(200)
        handler.send_header("content-type", "application/json")
        handler.send_header("content-length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    @staticmethod
    def refuse(handler, api, status):
        error = {"type": "invalid_request_error", "message": FAILURE_MESSAGE}
        body = {"type": "error", "error": error} if api == "anthropic" else {"error": {**error, "code": None}}
        data = json.dumps(body).encode()
        handler.send_response(status)
        handler.send_header("content-type", "application/json")
        handler.send_header("content-length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    @staticmethod
    def stream(handler, events):
        handler.send_response(200)
        handler.send_header("content-type", "text/event-stream")
        handler.end_headers()
        for name, data in events:
            handler.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
            handler.wfile.flush()

    def anthropic(self, handler, reply, body):
        usage = {"input_tokens": USAGE["input"] - USAGE["cached_input"],
                 "cache_read_input_tokens": USAGE["cached_input"],
                 "cache_creation_input_tokens": 0, "output_tokens": USAGE["output"]}
        if "tool" in reply:
            block = {"type": "tool_use", "id": f"toolu_fake{time.time_ns()}", "name": reply["tool"],
                     "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(reply["input"])}
            stop, content = "tool_use", [{**block, "input": reply["input"]}]
        else:
            block, stop = {"type": "text", "text": ""}, "end_turn"
            delta = {"type": "text_delta", "text": reply["text"]}
            content = [{"type": "text", "text": reply["text"]}]
        message = {"id": "msg_fake", "type": "message", "role": "assistant",
                   "model": body.get("model", "fake"), "content": [], "stop_reason": None,
                   "stop_sequence": None, "usage": usage}
        if not body.get("stream"):
            return self.reply_json(handler, {**message, "content": content, "stop_reason": stop})
        self.stream(handler, [
            ("message_start", {"type": "message_start", "message": message}),
            ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                               "usage": {"output_tokens": USAGE["output"]}}),
            ("message_stop", {"type": "message_stop"}),
        ])

    def openai(self, handler, reply):
        rid = f"resp_fake{time.time_ns()}"
        if "tool" in reply:
            item = {"type": "function_call", "id": f"fc_{rid}", "call_id": f"call_{rid}",
                    "name": reply["tool"], "arguments": json.dumps(reply["input"])}
        elif "custom_tool" in reply:
            item = {"type": "custom_tool_call", "id": f"ctc_{rid}", "call_id": f"call_{rid}",
                    "name": reply["custom_tool"], "input": reply["input"]}
        else:
            item = {"type": "message", "id": f"msg_{rid}", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": reply["text"], "annotations": []}]}
        usage = {"input_tokens": USAGE["input"], "input_tokens_details": {"cached_tokens": USAGE["cached_input"]},
                 "output_tokens": USAGE["output"],
                 "output_tokens_details": {"reasoning_tokens": USAGE["reasoning"]},
                 "total_tokens": USAGE["input"] + USAGE["output"]}
        self.stream(handler, [
            ("response.created", {"type": "response.created", "response": {"id": rid}}),
            ("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": item}),
            ("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item}),
            ("response.completed", {"type": "response.completed", "response": {"id": rid, "usage": usage}}),
        ])
