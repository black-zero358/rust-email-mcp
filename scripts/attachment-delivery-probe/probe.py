"""Synthetic-only, loopback MCP/Bearer download probe. Run with uv; never uses mail.

The small HTTP implementation exercises JSON Streamable HTTP, not OAuth or every
MCP feature. It deliberately has no production configuration or token discovery.
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import platform
import secrets
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.parse import quote, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener
from xml.sax.saxutils import escape
from zipfile import ZIP_STORED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parent
INLINE_LIMIT = 512 * 1024
WIRE_LIMIT = 2 * 1024 * 1024
REQUEST_LIMIT = 64 * 1024
VERSIONS = ("2025-06-18", "2025-11-25")


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def digest(value):
    return hashlib.sha256(value).hexdigest()


def xlsx_bytes():
    """A deterministic OOXML input table; the client computes totals itself."""
    rows = [["项目", "数量", "金额"], ["一", 3, 120.5], ["二", 2, 79.25], ["退款", 1, -5.75]]
    cells = []
    for r, row in enumerate(rows, 1):
        body = []
        for c, val in zip("ABC", row):
            body.append(
                f'<c r="{c}{r}" t="inlineStr"><is><t>{escape(val)}</t></is></c>'
                if isinstance(val, str)
                else f'<c r="{c}{r}"><v>{val}</v></c>'
            )
        cells.append(f'<row r="{r}">{"".join(body)}</row>')
    parts = {
        "[Content_Types].xml": '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
        "_rels/.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="明细" sheetId="1" r:id="rId1"/></sheets></workbook>',
        "xl/_rels/workbook.xml.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>',
        "xl/worksheets/sheet1.xml": '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>' + "".join(cells) + "</sheetData></worksheet>",
    }
    output = io.BytesIO()
    with ZipFile(output, "w", compression=ZIP_STORED) as archive:
        for name, content in parts.items():
            entry = ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            entry.compress_type = ZIP_STORED
            entry.external_attr = 0o600 << 16
            archive.writestr(entry, content.encode())
    return output.getvalue()


def pdf_bytes():
    paragraph = "Synthetic attachment probe. Project Cedar has 7 items. Verification code: CEDAR-729."
    stream = f"BT /F1 11 Tf 40 760 Td ({paragraph}) Tj ET".encode()
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream",
    ]
    output = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for index, obj in enumerate(objects, 1):
        offsets.append(len(output))
        output.extend(f"{index} 0 obj\n".encode() + obj + b"\nendobj\n")
    start = len(output)
    output.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        output.extend(f"{offset:010d} 00000 n \n".encode())
    output.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n".encode())
    return bytes(output)


def padded_csv(size):
    prefix = b"id,amount,note\r\nlarge,42,"
    return prefix + b"x" * (size - len(prefix) - 2) + b"\r\n"


def fixtures():
    entries = [
        ("utf8-csv", "汇总 & # [验收] 100%.csv", "text/csv", "项目,数量,金额\r\n一,3,120.50\r\n二,2,79.25\r\n退款,1,-5.75\r\n".encode()),
        ("workbook", "验收明细.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", xlsx_bytes()),
        ("paragraph", "验收段落.pdf", "application/pdf", pdf_bytes()),
        ("same-name-a", "同名.csv", "text/csv", b"id,amount\r\na,11\r\n"),
        ("same-name-b", "同名.csv", "text/csv", b"id,amount\r\nb,29\r\n"),
        ("legacy-over", "128KiB-plus-one.csv", "text/csv", padded_csv(128 * 1024 + 1)),
        ("inline-exact", "512KiB-exact.csv", "text/csv", padded_csv(INLINE_LIMIT)),
        ("inline-over", "512KiB-plus-one.csv", "text/csv", padded_csv(INLINE_LIMIT + 1)),
    ]
    return {key: {"fixture_id": key, "filename": name, "mime_type": mime, "decoded_size": len(data), "sha256": digest(data), "bytes": data} for key, name, mime, data in entries}


def public_fixture(fixture):
    return {key: value for key, value in fixture.items() if key != "bytes"}


def parse_fixture(key, data):
    if key == "workbook":
        from openpyxl import load_workbook

        workbook = load_workbook(io.BytesIO(data), read_only=True, data_only=True)
        try:
            sheet = workbook["明细"]
            rows = list(sheet.iter_rows(min_row=2, values_only=True))
            result = {"worksheet": sheet.title, "rows": len(rows), "quantity_sum": sum(row[1] for row in rows), "amount_sum": sum(row[2] for row in rows)}
        finally:
            workbook.close()
        assert result == {"worksheet": "明细", "rows": 3, "quantity_sum": 6, "amount_sum": 194.0}
        return result
    if key == "paragraph":
        from pypdf import PdfReader

        reader = PdfReader(io.BytesIO(data), strict=True)
        text = reader.pages[0].extract_text()
        assert "Project Cedar has 7 items." in text and "CEDAR-729" in text
        return {"pages": len(reader.pages), "verification_code": "CEDAR-729", "items": 7}
    csv.field_size_limit(WIRE_LIMIT)
    rows = list(csv.DictReader(io.StringIO(data.decode("utf-8"), newline="")))
    column = "金额" if key == "utf8-csv" else "amount"
    total = sum(float(row[column]) for row in rows)
    expected = {"utf8-csv": 194.0, "same-name-a": 11.0, "same-name-b": 29.0}.get(key, 42.0)
    assert total == expected
    return {"rows": len(rows), "amount_sum": total}


class ProbeState:
    def __init__(self, token, other_token=None, ttl=600):
        if not token or (other_token is not None and (not other_token or other_token == token)):
            raise ValueError("Distinct nonempty probe credentials are required")
        self.identities = [(token, "probe-owner"), *([(other_token, "probe-other")] if other_token else [])]
        self.acl = {"probe-owner": {"default"}, "probe-other": {"default"}}
        self.revoked = set()
        self.handles = {}
        self.fixture = fixtures()
        self.ttl = ttl
        self.base_url = ""
        self.lock = threading.Lock()

    def principal(self, header):
        if not header.startswith("Bearer "):
            return None
        supplied = header[7:]
        for token, principal in self.identities:
            if hmac.compare_digest(supplied.encode(), token.encode()) and principal not in self.revoked:
                return principal
        return None

    def prepare(self, principal, key, account):
        if account not in self.acl.get(principal, set()) or key not in self.fixture:
            return None
        with self.lock:
            now = time.monotonic()
            self.handles = {k: v for k, v in self.handles.items() if v[3] > now}
            if len(self.handles) >= 64:
                return None
            handle = secrets.token_urlsafe(24)
            self.handles[handle] = (principal, account, key, now + self.ttl)
        return handle

    def lookup(self, handle, principal):
        with self.lock:
            value = self.handles.get(handle)
            if value and value[0] == principal and value[1] in self.acl.get(principal, set()) and value[3] > time.monotonic():
                return self.fixture[value[2]]
        return None


def tool_list():
    annotations = {"readOnlyHint": True, "destructiveHint": False, "openWorldHint": False, "idempotentHint": True}
    return [
        {"name": "list_probe_fixtures", "description": "List synthetic fixtures only; no email access.", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}, "annotations": annotations},
        {"name": "prepare_probe_attachment", "description": "Return an isolated fixture as authenticated binary URL, MCP resource link, or bounded Base64.", "inputSchema": {"type": "object", "properties": {"fixture_id": {"type": "string", "enum": list(fixtures())}, "account_id": {"type": "string", "default": "default"}, "mode": {"type": "string", "enum": ["binary", "resource", "inline"]}}, "required": ["fixture_id", "mode"], "additionalProperties": False}, "annotations": annotations},
    ]


class ProbeHandler(BaseHTTPRequestHandler):
    server_version = "SyntheticAttachmentProbe/0.1"

    def log_message(self, *_):
        pass  # Never log headers, bearer tokens, URLs, filenames or request bodies.

    @property
    def state(self):
        return self.server.state

    def send_bytes(self, status, data=b"", mime="application/json", extra=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(data)

    def authenticate(self):
        principal = self.state.principal(self.headers.get("Authorization", ""))
        if principal is None:
            self.send_bytes(401, encoded({"error": "unauthorized"}), extra={"WWW-Authenticate": "Bearer"})
        elif self.headers.get("Origin") not in (None, self.state.base_url):
            self.send_bytes(403, encoded({"error": "origin_rejected"}))
            principal = None
        return principal

    def do_GET(self):
        principal = self.authenticate()
        if principal is None:
            return
        path = urlsplit(self.path)
        if path.query or path.fragment:
            self.send_bytes(400, encoded({"error": "query_not_supported"}))
            return
        if path.path.startswith("/attachments/"):
            fixture = self.state.lookup(path.path.removeprefix("/attachments/"), principal)
            if fixture is None:
                self.send_bytes(404, encoded({"error": "not_found"}))
                return
            self.send_bytes(200, fixture["bytes"], "application/octet-stream", {"Content-Disposition": "attachment; filename=\"attachment.bin\"; filename*=UTF-8''" + quote(fixture["filename"], safe="")})
        else:
            self.send_bytes(405 if path.path == "/mcp" else 404)

    def do_DELETE(self):
        if self.authenticate() is not None:
            self.send_bytes(405)

    def do_POST(self):
        principal = self.authenticate()
        if principal is None:
            return
        if self.path != "/mcp":
            self.send_bytes(404)
            return
        # The listener is loopback-only. A reverse proxy enforces public Host/TLS.
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if size <= 0 or size > REQUEST_LIMIT:
                self.send_bytes(413)
                return
            request = json.loads(self.rfile.read(size))
            if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
                raise ValueError()
            request_id = request.get("id")
            if len(encoded(request_id)) > 1024:
                self.send_bytes(413)
                return
            method = request["method"]
            params = request.get("params", {})
            if not isinstance(params, dict):
                raise ValueError()
        except (ValueError, KeyError, TypeError):
            self.send_bytes(400, encoded({"error": "invalid_request"}))
            return
        if "id" not in request:
            self.send_bytes(202)
            return
        if method == "initialize":
            version = params.get("protocolVersion")
            result = {"protocolVersion": version if version in VERSIONS else VERSIONS[0], "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "synthetic-attachment-probe", "version": "0.1.0"}}
        elif method == "ping":
            result = {}
        elif method == "tools/list":
            result = {"tools": tool_list()}
        elif method == "resources/list":
            result = {"resources": []}
        elif method == "resources/read":
            uri = params.get("uri", "")
            fixture = self.state.lookup(uri.removeprefix("probe://attachments/"), principal) if isinstance(uri, str) and uri.startswith("probe://attachments/") else None
            if fixture is None:
                self.rpc_error(request_id, -32002, "Resource not found")
                return
            result = {"contents": [{"uri": uri, "mimeType": fixture["mime_type"], "blob": base64.b64encode(fixture["bytes"]).decode()}]}
        elif method == "tools/call":
            result = self.call_tool(principal, params)
        else:
            self.rpc_error(request_id, -32601, "Method not found")
            return
        response = encoded({"jsonrpc": "2.0", "id": request_id, "result": result})
        if len(response) > WIRE_LIMIT:
            self.rpc_error(request_id, -32000, "Response budget exceeded")
            return
        self.send_bytes(200, response)

    def rpc_error(self, request_id, code, message):
        self.send_bytes(200, encoded({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}))

    def call_tool(self, principal, params):
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "list_probe_fixtures" and args == {}:
            data = {"fixtures": [public_fixture(f) for f in self.state.fixture.values()], "inline_limit_bytes": INLINE_LIMIT}
        elif name == "prepare_probe_attachment" and isinstance(args, dict) and set(args) <= {"fixture_id", "account_id", "mode"}:
            key, mode = args.get("fixture_id"), args.get("mode")
            account = args.get("account_id", "default")
            if not isinstance(key, str) or not isinstance(account, str) or mode not in ("binary", "resource", "inline"):
                return self.tool_error("INVALID_INPUT")
            handle = self.state.prepare(principal, key, account)
            if handle is None:
                return self.tool_error("NOT_FOUND")
            fixture = self.state.fixture[key]
            data = {**public_fixture(fixture), "account_id": account, "expires_in_seconds": self.state.ttl}
            if mode == "inline":
                if fixture["decoded_size"] > INLINE_LIMIT:
                    return self.tool_error("LIMIT_EXCEEDED")
                data["base64"] = base64.b64encode(fixture["bytes"]).decode()
            elif mode == "binary":
                data["delivery"] = {"method": "GET", "download_url": self.state.base_url + "/attachments/" + handle, "authentication": "same_principal_bearer"}
            else:
                uri = "probe://attachments/" + handle
                data["delivery"] = {"method": "resources/read", "uri": uri}
                return {"content": [{"type": "resource_link", "uri": uri, "name": fixture["filename"], "mimeType": fixture["mime_type"], "size": fixture["decoded_size"]}], "structuredContent": {"data": data, "request_id": "synthetic-probe"}, "isError": False}
        else:
            return self.tool_error("INVALID_INPUT")
        envelope = {"data": data, "request_id": "synthetic-probe"}
        return {"content": [{"type": "text", "text": encoded(envelope).decode()}], "structuredContent": envelope, "isError": False}

    @staticmethod
    def tool_error(code):
        envelope = {"error": {"code": code, "retryable": False}, "request_id": "synthetic-probe"}
        return {"content": [{"type": "text", "text": encoded(envelope).decode()}], "structuredContent": envelope, "isError": True}


class NoRedirects(HTTPRedirectHandler):
    def redirect_request(self, *_):
        return None


def validate_url(url):
    parts = urlsplit(url)
    if parts.username or parts.password or parts.query or parts.fragment:
        raise ValueError("Use an origin URL without credentials, query or fragment")
    if parts.scheme != "https" and not (parts.scheme == "http" and parts.hostname in ("127.0.0.1", "localhost", "::1")):
        raise ValueError("HTTPS is required outside loopback")
    if parts.path not in ("", "/"):
        raise ValueError("Probe base URL must be an origin")
    return url.rstrip("/")


def http(url, token=None, body=None):
    headers = {"Accept": "application/json, text/event-stream", "MCP-Protocol-Version": VERSIONS[0]}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
    if body is not None:
        headers["Content-Type"] = "application/json"
    request = Request(url, data=encoded(body) if body is not None else None, headers=headers)
    try:
        with build_opener(NoRedirects).open(request, timeout=10) as response:
            data = response.read(WIRE_LIMIT + 1)
            if len(data) > WIRE_LIMIT:
                raise ValueError("HTTP response budget exceeded")
            return response.status, dict(response.headers), data
    except HTTPError as error:
        # Never include the exception string: it may contain a URL.
        return error.code, dict(error.headers), error.read(REQUEST_LIMIT)


def rpc(base, token, method, params=None):
    status, _, data = http(base + "/mcp", token, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}})
    assert status == 200, f"MCP HTTP status {status}"
    return json.loads(data)


def prepare(base, token, key, mode):
    return rpc(base, token, "tools/call", {"name": "prepare_probe_attachment", "arguments": {"fixture_id": key, "mode": mode}})["result"]


def client_checks(base, token, output):
    init = rpc(base, token, "initialize", {"protocolVersion": VERSIONS[0], "capabilities": {}, "clientInfo": {"name": "python-stdlib-probe", "version": "0.1.0"}})
    assert init["result"]["protocolVersion"] == VERSIONS[0]
    status, _, _ = http(base + "/mcp", token, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    assert status == 202
    tools = rpc(base, token, "tools/list")["result"]["tools"]
    assert {tool["name"] for tool in tools} == {"list_probe_fixtures", "prepare_probe_attachment"}
    catalog = rpc(base, token, "tools/call", {"name": "list_probe_fixtures", "arguments": {}})["result"]["structuredContent"]["data"]
    local = fixtures()
    assert catalog["fixtures"] == [public_fixture(f) for f in local.values()]
    output.mkdir(parents=True, exist_ok=True)
    results = []
    for key, fixture in local.items():
        tool = prepare(base, token, key, "binary")
        data = tool["structuredContent"]["data"]
        url = data["delivery"]["download_url"]
        # Trust only the configured origin; never forward credentials to redirects.
        assert url.startswith(base + "/attachments/") and not urlsplit(url).query
        status, headers, binary = http(url, token)
        assert status == 200 and digest(binary) == fixture["sha256"] and binary == fixture["bytes"]
        assert headers["Content-Type"] == "application/octet-stream"
        assert headers["Cache-Control"] == "no-store" and headers["X-Content-Type-Options"] == "nosniff"
        assert "filename*=UTF-8''" + quote(fixture["filename"], safe="") in headers["Content-Disposition"]
        # Fixture id directory keeps duplicate names separate; fixture names are local constants.
        target = output / key / fixture["filename"]
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(binary)
        parsed = parse_fixture(key, target.read_bytes())
        linked = prepare(base, token, key, "resource")
        assert linked["content"][0]["type"] == "resource_link"
        uri = linked["content"][0]["uri"]
        resource = rpc(base, token, "resources/read", {"uri": uri})["result"]["contents"][0]
        resource_bytes = base64.b64decode(resource["blob"], validate=True)
        assert resource["mimeType"] == fixture["mime_type"] and resource_bytes == binary
        inline = prepare(base, token, key, "inline")
        if len(binary) <= INLINE_LIMIT:
            assert base64.b64decode(inline["structuredContent"]["data"]["base64"], validate=True) == binary
            inline_status = "passed"
        else:
            assert inline["isError"] and inline["structuredContent"]["error"]["code"] == "LIMIT_EXCEEDED"
            inline_status = "rejected_at_decoded_limit"
        results.append({**public_fixture(fixture), "binary_get": "passed", "resource_read": "passed", "inline": inline_status, "parsed": parsed})
    return {"protocol_version": VERSIONS[0], "fixtures": results}


def start_server(state, port=0, public_base=None):
    server = ThreadingHTTPServer(("127.0.0.1", port), ProbeHandler)
    server.daemon_threads = True
    server.state = state
    state.base_url = validate_url(public_base) if public_base else f"http://127.0.0.1:{server.server_port}"
    return server


def selftest(output):
    token, other = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
    state = ProbeState(token, other)
    server = start_server(state)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        base = state.base_url
        report = client_checks(base, token, output / "downloads")
        url = prepare(base, token, "utf8-csv", "binary")["structuredContent"]["data"]["delivery"]["download_url"]
        uri = prepare(base, token, "utf8-csv", "resource")["content"][0]["uri"]
        checks = {}
        for label, credential, expected in [("missing_bearer", None, 401), ("invalid_bearer", "synthetic-invalid", 401), ("different_principal", other, 404)]:
            status, _, _ = http(url, credential)
            assert status == expected
            checks[label] = status
        assert http(base + "/mcp", None, {"jsonrpc": "2.0", "id": 1, "method": "tools/list"})[0] == 401
        checks["mcp_missing_bearer"] = 401
        assert rpc(base, other, "resources/read", {"uri": uri})["error"]["code"] == -32002
        checks["resource_other_principal"] = -32002
        assert http(base + "/attachments/unknown", token)[0] == 404
        checks["unknown_handle"] = 404
        handle = url.rsplit("/", 1)[1]
        original = state.handles[handle]
        state.handles[handle] = (*original[:3], time.monotonic() - 1)
        assert http(url, token)[0] == 404
        checks["expired_handle"] = 404
        renewed = prepare(base, token, "utf8-csv", "binary")["structuredContent"]["data"]["delivery"]["download_url"]
        assert http(renewed, token)[0] == 200
        checks["prepare_after_expiry"] = 200
        state.acl["probe-owner"].clear()
        assert http(renewed, token)[0] == 404
        checks["account_revoked"] = 404
        state.acl["probe-owner"].add("default")
        state.revoked.add("probe-owner")
        assert http(renewed, token)[0] == 401
        checks["principal_revoked"] = 401
        state.revoked.clear()
        result = rpc(base, token, "tools/call", {"name": "prepare_probe_attachment", "arguments": {"fixture_id": "utf8-csv", "mode": "binary", "account_id": "other"}})["result"]
        assert result["structuredContent"]["error"]["code"] == "NOT_FOUND"
        checks["other_account_prepare"] = "NOT_FOUND"
        status, _, _ = http(base + "/mcp", token, {"jsonrpc": "2.0", "id": "x" * 1025, "method": "tools/list"})
        assert status == 413
        checks["oversized_rpc_id"] = 413
        report.update({"test": "isolated_synthetic_selftest", "python": platform.python_version(), "platform": platform.system(), "transport": "loopback HTTP JSON Streamable HTTP; no OAuth", "negative_checks": checks, "chatgpt_analysis": "requires_target_client_acceptance"})
        return report
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["generate", "selftest", "serve", "client"])
    parser.add_argument("--output", type=Path, default=ROOT / "generated")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--port", type=int, default=18767)
    parser.add_argument("--public-base-url")
    args = parser.parse_args()
    if args.command == "generate":
        args.output.mkdir(parents=True, exist_ok=True)
        for key, fixture in fixtures().items():
            destination = args.output / key / fixture["filename"]
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(fixture["bytes"])
        report = {"fixtures": [{**public_fixture(f), "parsed": parse_fixture(k, f["bytes"])} for k, f in fixtures().items()]}
    elif args.command == "selftest":
        report = selftest(args.output)
    else:
        token = os.environ.get("PROBE_BEARER_TOKEN")
        if not token:
            raise ValueError("Set PROBE_BEARER_TOKEN in the environment")
        if args.command == "serve":
            server = start_server(ProbeState(token, os.environ.get("PROBE_OTHER_BEARER_TOKEN")), args.port, args.public_base_url)
            print("Synthetic probe listening on loopback. Credentials and URLs are omitted.", flush=True)
            try:
                server.serve_forever()
            except KeyboardInterrupt:
                pass
            finally:
                server.server_close()
            return
        base = validate_url(os.environ.get("PROBE_BASE_URL", ""))
        report = client_checks(base, token, args.output / "downloads")
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"status": "passed", "fixture_count": len(report["fixtures"]), "report_written": bool(args.report)}, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Keep request URLs, credentials, fixture content and exception details out of logs.
        print(json.dumps({"status": "failed", "error_type": type(error).__name__}), file=sys.stderr)
        sys.exit(1)
