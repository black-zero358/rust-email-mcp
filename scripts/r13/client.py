"""Loopback HTTP and authenticated MCP requests for the synthetic load."""

from contextlib import closing
import hashlib
from http.client import HTTPConnection
import json
import socket
import time
from urllib.parse import urlsplit

from .fixture import AcceptanceError, require


MCP_VERSION = "2025-06-18"
MAX_RPC_BYTES = 2 * 1024 * 1024
BODY_CHUNK = 64 * 1024


class ToolError(AcceptanceError):
    def __init__(self, tool, code, elapsed):
        self.code = code
        self.elapsed = elapsed
        super().__init__(f"{tool} returned {code}")


class SmallWindowConnection(HTTPConnection):
    """Set the advertised TCP window before connecting, then park the body."""

    def connect(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            self.sock.settimeout(self.timeout)
            self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8192)
            if hasattr(socket, "TCP_WINDOW_CLAMP"):
                self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_WINDOW_CLAMP, 8192)
            self.sock.connect((self.host, self.port))
        except BaseException:
            self.sock.close()
            self.sock = None
            raise


class ParkedDownload:
    def __init__(self, connection, response):
        self.connection = connection
        self.response = response

    def close(self):
        self.response.close()
        self.connection.close()


class McpClient:
    def __init__(self, base, bearer, resource=None):
        origin = urlsplit(base)
        require(origin.scheme == "http" and origin.hostname == "127.0.0.1" and origin.port,
                "fixture HTTP endpoint must be IPv4 loopback")
        require(not origin.username and not origin.password and origin.path in ("", "/")
                and not origin.query and not origin.fragment, "invalid fixture HTTP origin")
        self.port = origin.port
        self.bearer = bearer
        self.origin = origin
        self.resource = urlsplit(resource) if resource else None
        self.sequence = 0
        self.session = None

    def _headers(self):
        return {"Authorization": f"Bearer {self.bearer}"}

    def _rpc(self, method, params=None, notification=False):
        headers = {
            **self._headers(),
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            "MCP-Protocol-Version": MCP_VERSION,
        }
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        self.sequence += 1
        body = {"jsonrpc": "2.0", "method": method}
        if not notification:
            body["id"] = self.sequence
        if params is not None:
            body["params"] = params
        started = time.monotonic()
        with closing(HTTPConnection("127.0.0.1", self.port, timeout=30)) as connection:
            connection.request("POST", "/mcp", json.dumps(body, ensure_ascii=False).encode(), headers)
            with connection.getresponse() as response:
                if notification:
                    require(response.status in (200, 202, 204), f"{method} HTTP {response.status}")
                    return None, time.monotonic() - started
                require(response.status == 200, f"{method} HTTP {response.status}")
                if method == "initialize":
                    self.session = response.getheader("Mcp-Session-Id")
                request_id = response.getheader("X-Request-Id")
                wire = response.read(MAX_RPC_BYTES + 1)
        elapsed = time.monotonic() - started
        require(len(wire) <= MAX_RPC_BYTES, "oversized MCP response")
        rpc = json.loads(wire)
        require(rpc.get("jsonrpc") == "2.0" and rpc.get("id") == self.sequence, "invalid MCP response ID")
        require("error" not in rpc, f"{method} produced a JSON-RPC error")
        result = rpc["result"]
        if method == "tools/call":
            result = result["structuredContent"]
            require(request_id and result.get("request_id") == request_id, "MCP request ID mismatch")
        return result, elapsed

    def initialize(self):
        result, _ = self._rpc("initialize", {
            "protocolVersion": MCP_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "r13-synthetic-acceptance", "version": "1"},
        })
        require(result.get("protocolVersion") == MCP_VERSION, "fixture did not negotiate the expected MCP version")
        self._rpc("notifications/initialized", notification=True)

    def call(self, tool, args):
        value, elapsed = self._rpc("tools/call", {"name": tool, "arguments": args})
        if "error" in value:
            raise ToolError(tool, value["error"].get("code", "UNSPECIFIED"), elapsed)
        return value["data"], elapsed

    def attachment_path(self, case):
        data, _ = self.call("prepare_attachment_download", {
            key: case[key] for key in ("account_id", "mailbox", "uid", "uid_validity", "attachment_index")
        })
        parsed = urlsplit(data["download_url"])
        require(data.get("method") == "GET" and parsed.path.startswith("/mcp/attachments/")
                and not parsed.query and not parsed.fragment and not parsed.username and not parsed.password,
                "invalid fixture attachment reference")
        allowed = {(self.origin.scheme, self.origin.netloc)}
        if self.resource:
            allowed.add((self.resource.scheme, self.resource.netloc))
        require((parsed.scheme, parsed.netloc) in allowed, "attachment reference escapes fixture origin")
        return parsed.path

    def park(self, path, case):
        connection = SmallWindowConnection("127.0.0.1", self.port, timeout=45)
        try:
            connection.request("GET", path, headers=self._headers())
            response = connection.getresponse()
            if response.status != 200 or int(response.getheader("Content-Length", "-1")) != case["bytes"]:
                response.close()
                raise AcceptanceError("parked GET did not return the full attachment length")
            return ParkedDownload(connection, response)
        except BaseException:
            connection.close()
            raise

    def expect_busy(self, path):
        with closing(HTTPConnection("127.0.0.1", self.port, timeout=8)) as connection:
            connection.request("GET", path, headers=self._headers())
            with connection.getresponse() as response:
                require(response.status == 429 and response.getheader("Retry-After") == "1",
                        "fifth attachment transfer did not return the capacity response")
                error = json.loads(response.read(4097))["error"]
                require(error.get("code") == "BUSY" and error.get("phase") == "transfer_admission",
                        "fifth GET did not fail at attachment transfer admission")

    def download(self, path, case, observe=None):
        digest = hashlib.sha256()
        received = 0
        started = time.monotonic()
        with closing(HTTPConnection("127.0.0.1", self.port, timeout=45)) as connection:
            connection.request("GET", path, headers=self._headers())
            with connection.getresponse() as response:
                require(response.status == 200 and int(response.getheader("Content-Length", "-1")) == case["bytes"],
                        "attachment GET failed or declared a different length")
                if observe:
                    observe()
                for chunk in iter(lambda: response.read(BODY_CHUNK), b""):
                    received += len(chunk)
                    require(received <= case["bytes"], "attachment exceeded the fixture length")
                    digest.update(chunk)
        require(received == case["bytes"] and digest.hexdigest() == case["sha256"],
                "attachment content did not match the synthetic fixture")
        return time.monotonic() - started
