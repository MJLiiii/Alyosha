"""A scriptable stdio MCP server for Alyosha's tests.

Tools: echo, image, fail (isError), big (50 KB of text), sleep (never
answers until cancelled), exit (the process exits), env (reports its
environment and working directory), get.item, send, and one tool whose name
is too long. Cancellations it receives are appended to $FAKE_MCP_LOG.
"""

import json
import os
import sys

TOOLS = [
    {"name": "echo", "description": "Echo the text.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "image", "description": "Return an image.", "inputSchema": {"type": "object"}},
    {"name": "fail", "description": "Fail.", "inputSchema": {"type": "object"}},
    {"name": "big", "description": "Return 50 KB.", "inputSchema": {"type": "object"}},
    {"name": "sleep", "description": "Never answer.", "inputSchema": {"type": "object"}},
    {"name": "exit", "description": "Exit the server.", "inputSchema": {"type": "object"}},
    {"name": "env", "description": "Report the environment.", "inputSchema": {"type": "object"}},
    {"name": "get.item", "title": "Get an item", "inputSchema": {"type": "object"}},
    {"name": "send", "description": "Send a message.", "inputSchema": {"type": "object", "properties": {"to": {"type": "string"}, "body": {"type": "string"}}, "required": ["to", "body"]}},
    {"name": "x" * 60, "description": "Too long once prefixed.", "inputSchema": {"type": "object"}},
]


def log(text):
    path = os.environ.get("FAKE_MCP_LOG")
    if path:
        with open(path, "a") as file:
            file.write(text + "\n")


def reply(message_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message_id, "result": result}) + "\n")
    sys.stdout.flush()


def text(value):
    return {"content": [{"type": "text", "text": value}]}


def main():
    sys.stderr.write("fake server starting " + "e" * 5000 + "\n")
    sys.stderr.flush()
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        message_id = message.get("id")
        if method == "initialize":
            reply(message_id, {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}})
        elif method == "tools/list":
            cursor = (message.get("params") or {}).get("cursor")
            if cursor is None:
                reply(message_id, {"tools": TOOLS[:4], "nextCursor": "page2"})
            else:
                reply(message_id, {"tools": TOOLS[4:]})
        elif method == "notifications/cancelled":
            log("cancelled " + str(message["params"]["requestId"]))
        elif method == "tools/call":
            name = message["params"]["name"]
            arguments = message["params"].get("arguments") or {}
            log("call " + name)
            if name == "echo":
                reply(message_id, text(arguments["text"]))
            elif name == "image":
                reply(message_id, {"content": [{"type": "image", "mimeType": "image/png", "data": "AAAA"}]})
            elif name == "fail":
                reply(message_id, {"content": [{"type": "text", "text": "it broke"}], "isError": True})
            elif name == "big":
                reply(message_id, text("".join(str(i % 10) for i in range(50_000))))
            elif name == "sleep":
                pass
            elif name == "exit":
                sys.exit(0)
            elif name == "env":
                names = sorted(os.environ)
                names = [n for n in names if n not in ("PWD", "SHLVL", "_", "LC_CTYPE")]
                reply(message_id, text(json.dumps({"env": names, "cwd": os.getcwd()})))
            elif name == "send":
                reply(message_id, text("sent to " + arguments["to"]))
            else:
                reply(message_id, text(name))


main()
