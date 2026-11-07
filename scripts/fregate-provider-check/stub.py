"""Stub of the Anthropic Messages API for the Fregate provider live check.

Answers POST /v1/messages with server-sent events shaped as the Claude API
documents them, picks the scripted case from the text of the last user turn,
and appends every request (headers and body) to requests.jsonl.

Cases (a marker in the last user text):
  CASE:FALLBACK  a mid-stream refusal fallback (claude-opus-5-5 -> claude-opus-5)
  CASE:REFUSE    the whole chain declines: fallback marker, then refusal (cyber)
  CASE:CUTOFF    max_tokens below 32000 is cut off; 32000 or more answers
  CASE:CUTTWICE  always cut off
  CASE:ORPHAN    a thinking block and a bash tool call that sleeps
  otherwise      a thinking block and a short text
A last user turn holding only tool results gets a short text.
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1]
PORT = int(sys.argv[2])
lock = threading.Lock()
counter = [0]


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n"


def message_start(msg_id, model, usage, binding):
    message = {
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": None,
        "usage": usage,
    }
    if binding:
        message["input_transformations"] = []
    return sse("message_start", {"type": "message_start", "message": message})


def block(index, content_block, deltas=()):
    out = sse("content_block_start", {"type": "content_block_start", "index": index, "content_block": content_block})
    for delta in deltas:
        out += sse("content_block_delta", {"type": "content_block_delta", "index": index, "delta": delta})
    out += sse("content_block_stop", {"type": "content_block_stop", "index": index})
    return out


def thinking(index, signature, text=""):
    deltas = []
    if text:
        deltas.append({"type": "thinking_delta", "thinking": text})
    deltas.append({"type": "signature_delta", "signature": signature})
    return block(index, {"type": "thinking", "thinking": "", "signature": ""}, deltas)


def text(index, value):
    return block(index, {"type": "text", "text": ""}, [{"type": "text_delta", "text": value}])


def end(stop_reason, usage, stop_details=None):
    return sse("message_delta", {
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": None, "stop_details": stop_details},
        "usage": usage,
    }) + sse("message_stop", {"type": "message_stop"})


def last_user_text(body):
    for message in reversed(body.get("messages", [])):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            return content, False
        texts = [b.get("text", "") for b in content if b.get("type") == "text"]
        has_result = any(b.get("type") == "tool_result" for b in content)
        return " ".join(texts), has_result and not texts
    return "", False


def answer(body, headers, n):
    model = body.get("model", "claude-opus-5-5")
    betas = headers.get("anthropic-beta", "")
    binding = "thinking-binding-controls-2026-08-01" in betas
    prompt, only_results = last_user_text(body)
    usage_in = {"input_tokens": 1200, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}
    msg_id = f"msg_stub_{n:03d}"
    if only_results:
        return (message_start(msg_id, model, usage_in, binding) + text(0, "Tool result received; the check is done.")
                + end("end_turn", {"output_tokens": 12}))
    if "CASE:FALLBACK" in prompt:
        usage = {
            "input_tokens": 1200, "output_tokens": 30, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "iterations": [
                {"type": "message", "model": model, "input_tokens": 1200, "output_tokens": 18, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                {"type": "fallback_message", "model": "claude-opus-5", "input_tokens": 1200, "output_tokens": 30, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
            ],
        }
        return (message_start(msg_id, model, usage_in, binding)
                + thinking(0, "sig-declined-attempt")
                + text(1, "Looking at the service. ")
                + block(2, {"type": "fallback", "from": {"model": model}, "to": {"model": "claude-opus-5"},
                            "trigger": {"type": "refusal", "category": "cyber"}})
                + text(3, "The port is closed.")
                + end("end_turn", usage))
    if "CASE:REFUSE" in prompt:
        usage = {
            "input_tokens": 1200, "output_tokens": 0,
            "iterations": [
                {"type": "message", "model": model, "input_tokens": 1200, "output_tokens": 0},
                {"type": "fallback_message", "model": "claude-opus-5", "input_tokens": 1200, "output_tokens": 0},
            ],
        }
        return (message_start(msg_id, "claude-opus-5", usage_in, binding)
                + block(0, {"type": "fallback", "from": {"model": model}, "to": {"model": "claude-opus-5"},
                            "trigger": {"type": "refusal", "category": "cyber"}})
                + end("refusal", usage, {"type": "refusal", "category": "cyber",
                                         "explanation": "This request was declined because it could enable cyber harm.",
                                         "recommended_model": None}))
    if "CASE:CUTOFF" in prompt or "CASE:CUTTWICE" in prompt:
        cut = "CASE:CUTTWICE" in prompt or body.get("max_tokens", 0) < 32000
        if cut:
            return (message_start(msg_id, model, usage_in, binding) + thinking(0, f"sig-cut-{n}")
                    + text(1, "The report sta") + end("max_tokens", {"output_tokens": body.get("max_tokens", 0)}))
        return (message_start(msg_id, model, usage_in, binding) + thinking(0, f"sig-full-{n}")
                + text(1, "The full report fits in the larger cap.") + end("end_turn", {"output_tokens": 20000}))
    if "CASE:ORPHAN" in prompt:
        return (message_start(msg_id, model, usage_in, binding)
                + thinking(0, "sig-orphan-call")
                + block(1, {"type": "tool_use", "id": "toolu_orphan", "name": "bash", "input": {}},
                        [{"type": "input_json_delta", "partial_json": json.dumps({"command": "sleep 917"})}])
                + end("tool_use", {"output_tokens": 40}))
    return (message_start(msg_id, model, usage_in, binding) + thinking(0, f"sig-default-{n}")
            + text(1, "Hello from the stub.") + end("end_turn", {"output_tokens": 8}))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        body = json.loads(raw)
        headers = {k.lower(): v for k, v in self.headers.items()}
        with lock:
            counter[0] += 1
            n = counter[0]
            with open(LOG, "a") as log:
                log.write(json.dumps({"n": n, "path": self.path, "headers": headers, "body": body}) + "\n")
        payload = answer(body, headers, n).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("request-id", f"req_stub_{n:03d}")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
