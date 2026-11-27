#!/usr/bin/env python3
"""Text clipboard bridge; run as the logged-in Wayland desktop user."""
import argparse
import hmac
import ipaddress
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAX_TEXT_BYTES = 64 * 1024
MAX_BODY_BYTES = 512 * 1024
# Same bounds the Windows bridge enforces before it will look at a request.
MAX_HEADER_BYTES = 8 * 1024
MAX_CONCURRENCY = 4
DEFAULT_REQUEST_DEADLINE_SECONDS = 5.0


def clipboard_environment():
    env = os.environ.copy()
    runtime = Path(env.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
    display = env.get("WAYLAND_DISPLAY", "")
    candidates = [runtime / display] if display else []
    candidates += sorted(runtime.glob("wayland-*"))
    for candidate in candidates:
        try:
            if stat.S_ISSOCK(candidate.stat().st_mode):
                env["XDG_RUNTIME_DIR"] = str(runtime)
                env["WAYLAND_DISPLAY"] = str(candidate)
                return env
        except FileNotFoundError:
            pass
    raise RuntimeError("No Wayland desktop is available")


def write_clipboard(text):
    # No shell, command interpolation, or keystrokes.
    subprocess.run(
        ["/usr/bin/wl-copy", "--type", "text/plain;charset=utf-8"],
        input=text.encode("utf-8"), env=clipboard_environment(),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        check=True, timeout=5,
    )


def read_clipboard():
    env = clipboard_environment()
    types = subprocess.run(
        ["/usr/bin/wl-paste", "--list-types"], env=env,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=4,
    )
    available = types.stdout.decode("utf-8", errors="replace").splitlines()
    mime = next((t for t in available if t.lower().startswith("text/plain")), None)
    if mime is None:
        mime = next((t for t in available if t in ("UTF8_STRING", "TEXT", "STRING")), None)
    if mime is None:
        return {"has_text": False, "text": ""}
    # Bound the read before decoding, and kill a stalled clipboard owner.
    proc = subprocess.Popen(
        ["/usr/bin/wl-paste", "--no-newline", "--type", mime], env=env,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    timer = threading.Timer(4, proc.kill)
    timer.start()
    try:
        data = proc.stdout.read(MAX_TEXT_BYTES + 1)
        if len(data) > MAX_TEXT_BYTES:
            raise OverflowError("Clipboard text exceeds the size limit")
        if proc.wait(timeout=1) != 0:
            raise RuntimeError("Clipboard owner did not provide text")
        text = data.decode("utf-8")
        if "\0" in text:
            raise ValueError("Clipboard contains null characters")
        return {"has_text": True, "text": text}
    finally:
        timer.cancel()
        if proc.poll() is None:
            proc.kill()
        proc.wait()
        proc.stdout.close()


class RequestDeadlineExceeded(Exception):
    """The absolute per-request deadline elapsed; the request must be dropped."""


class HeaderTooLarge(Exception):
    """The request header exceeded MAX_HEADER_BYTES."""


class DeadlineReader:
    """Buffered reader that puts an absolute deadline on the whole request.

    A per-recv socket timeout only bounds one read, so a peer trickling single
    bytes could otherwise keep one request (and with it a worker slot) alive
    indefinitely. Every read re-arms the socket timeout with the time actually
    left before the request deadline, and the header phase additionally carries
    a hard byte budget. Bytes are pulled with ``read1`` so a single underlying
    receive can never block past the deadline that was just armed.
    """

    def __init__(self, raw, connection, deadline):
        self._raw = raw
        self._connection = connection
        self._deadline = deadline
        self._header_bytes_left = MAX_HEADER_BYTES
        self._header_phase = True
        self._buffer = b""

    def end_header_phase(self):
        self._header_phase = False

    def _arm(self):
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise RequestDeadlineExceeded
        try:
            self._connection.settimeout(remaining)
        except OSError as exc:  # peer already gone
            raise RequestDeadlineExceeded from exc

    def _account(self, data):
        if not self._header_phase or not data:
            return
        self._header_bytes_left -= len(data)
        if self._header_bytes_left < 0:
            raise HeaderTooLarge

    def _fill(self):
        self._arm()
        chunk = self._raw.read1(65536)
        if not chunk:
            return False
        self._buffer += chunk
        return True

    def readline(self, limit=-1):
        while True:
            index = self._buffer.find(b"\n")
            if index >= 0:
                end = index + 1
                if limit is not None and limit > 0 and end > limit:
                    end = limit
                line, self._buffer = self._buffer[:end], self._buffer[end:]
                self._account(line)
                return line
            if limit is not None and limit > 0 and len(self._buffer) >= limit:
                line, self._buffer = self._buffer[:limit], self._buffer[limit:]
                self._account(line)
                return line
            if not self._fill():
                line, self._buffer = self._buffer, b""
                self._account(line)
                return line

    def read(self, size=-1):
        want = None if size is None or size < 0 else size
        chunks = []
        obtained = 0
        while want is None or obtained < want:
            if self._buffer:
                take = len(self._buffer) if want is None else min(want - obtained, len(self._buffer))
                chunk, self._buffer = self._buffer[:take], self._buffer[take:]
                self._account(chunk)
                chunks.append(chunk)
                obtained += len(chunk)
                continue
            if not self._fill():
                break
        return b"".join(chunks)

    def close(self):
        self._raw.close()


class Handler(BaseHTTPRequestHandler):
    # Keep the historic HTTP/1.0 semantics: one request per connection, the
    # response closes it. The absolute deadline below is therefore per request.
    protocol_version = "HTTP/1.0"

    def setup(self):
        super().setup()
        deadline = time.monotonic() + self.server.bridge_config["request_deadline_seconds"]
        self.request_deadline = deadline
        self.rfile = DeadlineReader(self.rfile, self.connection, deadline)

    def handle_one_request(self):
        try:
            super().handle_one_request()
        except (RequestDeadlineExceeded, TimeoutError):
            # Absolute deadline (or a stalled socket) hit: answer and drop.
            self.close_connection = True
            self._reply_quietly(408)
        except HeaderTooLarge:
            self.close_connection = True
            self._reply_quietly(431)

    def _reply_quietly(self, status):
        try:
            self.reply(status)
        except OSError:
            pass

    def parse_request(self):
        parsed = super().parse_request()
        self.rfile.end_header_phase()
        return parsed

    def log_message(self, *_args):
        pass  # Never record clipboard contents or Authorization headers.

    def reply(self, status, payload=None):
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8") if payload is not None else b""
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def request_body_length(self):
        """Return the framed body length, or -1 when the framing is unsafe."""
        lengths = self.headers.get_all("Content-Length") or []
        if len(lengths) > 1:
            return -1  # duplicate framing headers are a smuggling shape
        if not lengths:
            return 0
        value = lengths[0].strip()
        if not value.isdigit():
            return -1
        length = int(value)
        if length > MAX_BODY_BYTES:
            return -2
        return length

    def authorized(self):
        config = self.server.bridge_config
        if self.client_address[0] not in config["allowed_ips"]:
            self.reply(403)
            return False
        authorization = self.headers.get("Authorization", "")
        if not hmac.compare_digest(authorization.encode(), ("Bearer " + config["token"]).encode()):
            self.reply(401)
            return False
        if self.path != "/clipboard":
            self.reply(404)
            return False
        if self.headers.get("Transfer-Encoding"):
            self.reply(415)
            return False
        return True

    def do_GET(self):
        if not self.authorized():
            return
        if self.request_body_length() != 0:
            return self.reply(400)
        try:
            payload = read_clipboard()
        except OverflowError:
            return self.reply(413)
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError):
            return self.reply(503)
        self.reply(200, payload)

    def do_POST(self):
        if not self.authorized():
            return
        if self.headers.get_content_type() != "application/json":
            return self.reply(415)
        length = self.request_body_length()
        if length == -1:
            return self.reply(400)
        if length == -2 or length == 0:
            return self.reply(413 if length == -2 else 400)
        try:
            body = self.rfile.read(length)
            if len(body) != length:
                return self.reply(400)
            payload = json.loads(body)
            text = payload.get("text") if isinstance(payload, dict) else None
            if not isinstance(text, str) or "\0" in text or len(text.encode("utf-8")) > MAX_TEXT_BYTES:
                return self.reply(400)
        except (ValueError, UnicodeError, socket.timeout):
            return self.reply(400)
        try:
            write_clipboard(text)
        except (OSError, RuntimeError, subprocess.SubprocessError):
            return self.reply(503)
        self.reply(204)


_STATUS_403 = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
_STATUS_503 = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"


class BridgeServer(ThreadingHTTPServer):
    """Peer filtering before parsing, bounded workers, daemon request threads."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address, config):
        self.bridge_config = config
        self._slots = threading.BoundedSemaphore(config["max_concurrency"])
        super().__init__(address, Handler)

    def verify_request(self, request, client_address):
        # Accept-stage peer check: a peer outside the allowlist never gets its
        # request line parsed, so it cannot occupy a worker with a slow header.
        if client_address[0] in self.bridge_config["allowed_ips"]:
            return True
        try:
            request.sendall(_STATUS_403)
        except OSError:
            pass
        return False

    def process_request(self, request, client_address):
        if not self._slots.acquire(blocking=False):
            try:
                request.sendall(_STATUS_503)
            except OSError:
                pass
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except Exception:
            self._slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._slots.release()


def validate_config(config):
    """Validate everything before a listener or the clipboard is touched."""
    if not isinstance(config, dict):
        raise SystemExit("Clipboard bridge config must be a JSON object")
    token = config.get("token", "")
    if not isinstance(token, str) or len(token) < 32 or any(c not in "0123456789abcdefABCDEF" for c in token):
        raise SystemExit("A random hexadecimal token of at least 32 characters is required")
    allowed = config.get("allowed_ips")
    if not isinstance(allowed, list) or not allowed:
        raise SystemExit("allowed_ips must list at least one web server address")
    peers = []
    for entry in allowed:
        if not isinstance(entry, str):
            raise SystemExit("allowed_ips entries must be strings")
        try:
            peers.append(str(ipaddress.ip_address(entry)))
        except ValueError:
            raise SystemExit("allowed_ips entries must be literal IP addresses") from None
    if len(set(peers)) != len(peers):
        raise SystemExit("allowed_ips contains duplicates")
    bind = config.get("bind")
    try:
        ipaddress.ip_address(bind)
    except (ValueError, TypeError):
        raise SystemExit("bind must be a literal IP address") from None
    port = config.get("port", 47991)
    if not isinstance(port, int) or isinstance(port, bool) or not 1024 <= port <= 65535:
        raise SystemExit("port must be an integer between 1024 and 65535")
    concurrency = config.get("max_concurrency", MAX_CONCURRENCY)
    if not isinstance(concurrency, int) or isinstance(concurrency, bool) or not 1 <= concurrency <= 32:
        raise SystemExit("max_concurrency must be an integer between 1 and 32")
    deadline = config.get("request_deadline_seconds", DEFAULT_REQUEST_DEADLINE_SECONDS)
    if not isinstance(deadline, (int, float)) or isinstance(deadline, bool) or not 0.5 <= deadline <= 30:
        raise SystemExit("request_deadline_seconds must be between 0.5 and 30")
    return {
        "bind": bind,
        "port": port,
        "allowed_ips": peers,
        "token": token,
        "max_concurrency": concurrency,
        "request_deadline_seconds": float(deadline),
    }


def check_config_permissions(path):
    """The bearer token lives in this file; refuse group/world readable modes."""
    mode = stat.S_IMODE(path.stat().st_mode)
    if mode & 0o077:
        raise SystemExit(
            f"{path} is accessible by other users (mode {mode:04o}); run: chmod 600 {path}"
        )


def create_server(config):
    settings = validate_config(config)
    return BridgeServer((settings["bind"], settings["port"]), settings)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument(
        "--check-config",
        action="store_true",
        help="validate the configuration and exit without binding or reading the clipboard",
    )
    args = parser.parse_args()
    check_config_permissions(args.config)
    config = json.loads(args.config.read_text())
    settings = validate_config(config)
    if args.check_config:
        print("configuration OK")
        return
    server = BridgeServer((settings["bind"], settings["port"]), settings)
    server.serve_forever()


if __name__ == "__main__":
    main()
