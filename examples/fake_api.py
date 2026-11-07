#!/usr/bin/env python3
"""A local stand-in for POST /v1/messages, for measuring without paid calls.

Every request is appended to --log as one JSON line: path, size of its messages
in bytes, whether it was refused, and the body. How to use it, on this harness
and on Claude Code, is in docs/agents/measuring.md.

What comes back depends on the last user line that is not a carried message:

  read X      a read_file call on X; after its result, "done"
  edit X      an edit_file call creating X; after its result, "done"
  big         95,000 input tokens reported for the turn
  long        30,000 input tokens, 95,000 once a tool result has come back
  huge        a 40,000-byte reply
  overflow    in any user message: every request refused as too long
  sumfail     a later summary request gets a 400
  sumtool     a later summary request gets a tool call

Anything else gets "ok", and a summary request gets a summary.
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

SUMMARY_REQUEST = "The conversation is about"
CARRIED = ("<", "[Image:")
SUMMARY = "<summary>SUMMARY OF WORK</summary>"


def texts(message):
    content = message["content"]
    if isinstance(content, str):
        return [content]
    return [block["text"] for block in content if block.get("type") == "text"]


def last_user_line(messages):
    for message in reversed(messages):
        if message["role"] != "user":
            continue
        lines = [
            text
            for text in texts(message)
            if not text.startswith((SUMMARY_REQUEST, *CARRIED))
        ]
        if lines:
            return lines[-1]
    return ""


def events(blocks, stop, input_tokens):
    out = [
        {
            "type": "message_start",
            "message": {
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": "claude-haiku-5-5",
                "content": [],
                "stop_reason": None,
                "stop_sequence": None,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0},
            },
        }
    ]
    for index, block in enumerate(blocks):
        if block["type"] == "text":
            start = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": block["text"]}
        else:
            start = {
                "type": "tool_use",
                "id": block["id"],
                "name": block["name"],
                "input": {},
            }
            delta = {
                "type": "input_json_delta",
                "partial_json": json.dumps(block["input"]),
            }
        out.append(
            {"type": "content_block_start", "index": index, "content_block": start}
        )
        out.append({"type": "content_block_delta", "index": index, "delta": delta})
        out.append({"type": "content_block_stop", "index": index})
    out.append(
        {
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": None},
            "usage": {"output_tokens": 10},
        }
    )
    out.append({"type": "message_stop"})
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in out)


def text_reply(text, input_tokens):
    return events([{"type": "text", "text": text}], "end_turn", input_tokens)


def tool_reply(call_id, name, tool_input, input_tokens):
    block = {"type": "tool_use", "id": call_id, "name": name, "input": tool_input}
    return events([block], "tool_use", input_tokens)


class Handler(BaseHTTPRequestHandler):
    args = None
    calls = 0

    def log_message(self, *_):
        pass

    def send(self, status, content_type, data):
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def refuse(self, message):
        body = {
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        }
        self.send(400, "application/json", json.dumps(body).encode())

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["content-length"]))
        body = json.loads(raw)
        messages = body.get("messages", [])
        size = len(json.dumps(messages))
        line = last_user_line(messages)
        last = messages[-1] if messages else {"content": ""}
        is_summary = any(t.startswith(SUMMARY_REQUEST) for t in texts(last))
        answered = isinstance(last["content"], list) and any(
            block.get("type") == "tool_result" for block in last["content"]
        )
        overflow = any(
            "overflow" in text
            for message in messages
            if message["role"] == "user"
            for text in texts(message)
        )
        too_long = overflow or (self.args.limit and size > self.args.limit)

        with open(self.args.log, "a") as log:
            record = {"path": self.path, "size": size, "over": bool(too_long)}
            log.write(json.dumps({**record, "body": body}) + "\n")

        if too_long:
            detail = (
                f": {size} tokens > {self.args.limit} maximum"
                if self.args.report_tokens
                else ""
            )
            return self.refuse("prompt is too long" + detail)
        if (
            self.args.refuse_summary_with_results
            and is_summary
            and "tool_result" in raw.decode()
        ):
            return self.refuse("thinking blocks cannot be modified")
        if is_summary and "sumfail" in line:
            return self.refuse("fake refusal")

        Handler.calls += 1
        call_id = f"toolu_{Handler.calls}"
        tokens = 95_000 if "big" in line else 1_000
        if "long" in line:
            tokens = 95_000 if answered else 30_000

        if is_summary and "sumtool" in line:
            reply = tool_reply(call_id, "shell", {"command": "ls"}, tokens)
        elif is_summary:
            reply = text_reply(SUMMARY, tokens)
        elif answered:
            reply = text_reply("done", tokens)
        elif "read " in line:
            path = line.split("read ", 1)[1].split()[0]
            reply = tool_reply(call_id, "read_file", {"path": path}, tokens)
        elif "edit " in line:
            path = line.split("edit ", 1)[1].split()[0]
            tool_input = {"path": path, "old_string": "", "new_string": "edited\n"}
            reply = tool_reply(call_id, "edit_file", tool_input, tokens)
        elif line.endswith("huge"):
            reply = text_reply("Z" * 40_000, tokens)
        else:
            reply = text_reply("ok", tokens)
        self.send(200, "text/event-stream", reply.encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=18765)
    parser.add_argument("--log", required=True, help="file each request is appended to")
    parser.add_argument(
        "--limit", type=int, help="refuse a request whose messages exceed these bytes"
    )
    parser.add_argument(
        "--report-tokens",
        action="store_true",
        help="name the sizes in a too-long refusal",
    )
    parser.add_argument(
        "--refuse-summary-with-results",
        action="store_true",
        help="answer a summary request holding a tool result with another 400",
    )
    Handler.args = parser.parse_args()
    HTTPServer(("127.0.0.1", Handler.args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
