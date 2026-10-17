#!/usr/bin/env python3
"""Real native stdio -> real service -> local HTTP. No Firefox API mocks here.
Pass a directory containing matching debug/release rdm-native-host and rdm-service.
Loopback permission is test-only and scoped to these child processes.
"""
import http.server
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

BIN = Path(sys.argv[1]).resolve()
PAYLOAD = b"rdm-firefox-smoke\n" * 4096

class Handler(http.server.BaseHTTPRequestHandler):
    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(PAYLOAD)))
        self.end_headers()
    def do_GET(self):
        assert not any(self.headers.get(key) for key in ("Cookie", "Authorization", "Referer"))
        self.do_HEAD()
        try:
            for offset in range(0, len(PAYLOAD), 1024):
                self.wfile.write(PAYLOAD[offset:offset+1024])
                self.wfile.flush()
                time.sleep(.015)
        except (BrokenPipeError, ConnectionResetError):
            pass
    def log_message(self, *args):
        pass

def receive(stream, size):
    data = b""
    while len(data) < size:
        chunk = stream.recv(size-len(data))
        if not chunk:
            raise EOFError("truncated IPC response")
        data += chunk
    return data

with tempfile.TemporaryDirectory(prefix="rdm-firefox-test-") as name:
    root = Path(name)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    config = root / "config/rdm"
    config.mkdir(parents=True)
    downloads = root / "downloads"
    (config / "config.toml").write_text(f'connections = 1\ndownload_dir = "{downloads}"\nmax_retries = 0\nqueue_parallel = 1\n')
    env = dict(os.environ, HOME=str(root), XDG_RUNTIME_DIR=str(runtime), XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / "config"), RDM_ALLOW_PRIVATE="1")
    env.pop("RDM_ALLOW_PROXY", None)
    socket_path = runtime / "rdm/service.sock"
    def native(command):
        data = json.dumps({"version": 1, "command": command}).encode()
        process = subprocess.run([str(BIN / "rdm-native-host"), str(root / ".mozilla/native-messaging-hosts/rdm_firefox.json"), "rdm@delllatitude7490"], input=struct.pack("=I", len(data))+data, capture_output=True, env=env, timeout=15, check=True)
        assert not process.stderr, "host must not log payloads"
        assert len(process.stdout) >= 4
        size = struct.unpack("=I", process.stdout[:4])[0]
        assert size == len(process.stdout)-4
        return json.loads(process.stdout[4:])
    def ipc(command):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(str(socket_path))
            data = json.dumps({"version": 5, "body": command}).encode()
            stream.sendall(struct.pack(">I", len(data))+data)
            size = struct.unpack(">I", receive(stream, 4))[0]
            assert 0 < size <= 1024*1024
            response = json.loads(receive(stream, size))
            assert response["body"]["type"] == "snapshot", response
            return response["body"]["data"]
    def until(predicate):
        deadline = time.monotonic()+15
        while time.monotonic() < deadline:
            state = ipc({"command": "list"})
            if predicate(state):
                return state
            time.sleep(.03)
        raise AssertionError("service state did not reach expected condition")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        subprocess.run([str(BIN / "rdm-native-host"), "--install"], env=env, check=True, capture_output=True)
        manifest_path = root / ".mozilla/native-messaging-hosts/rdm_firefox.json"
        manifest = json.loads(manifest_path.read_text())
        assert manifest["path"] == str(BIN / "rdm-native-host")
        assert manifest["allowed_extensions"] == ["rdm@delllatitude7490"]
        assert native({"action": "status"})["code"] == "unavailable"
        assert not socket_path.exists(), "status must not start service"
        url = f"http://127.0.0.1:{server.server_port}/file.bin?token=test-only-secret"
        accepted = native({"action": "add", "url": url})
        assert accepted["code"] == "accepted", accepted
        assert "secret" not in json.dumps(accepted)
        queued = native({"action": "add", "url": url})
        assert queued["code"] == "queued", queued
        state = ipc({"command": "list"})
        assert state["jobs"][-1]["queued"]
        ipc({"command": "remove_queued", "data": {"instance": state["instance"], "id": queued["job_id"]}})
        state = until(lambda s: s["jobs"][0]["progress"]["downloaded"] > 0)
        before = state["jobs"][0]["progress"]["downloaded"]
        assert native({"action": "status"})["code"] == "active"
        state = until(lambda s: s["jobs"][0]["progress"]["downloaded"] > before)
        assert state["jobs"][0]["result"] is None, "service continues after short-lived bridge exits"
        state = until(lambda s: s["jobs"][0]["result"] is not None)
        assert "Completed" in state["jobs"][0]["result"], state
        assert (downloads / "file.bin").read_bytes() == PAYLOAD
        assert native({"action": "status"})["code"] == "ready"
        (downloads / "file.bin").write_bytes(b"keep existing output")
        assert native({"action": "add", "url": url})["code"] == "accepted"
        state = until(lambda s: len(s["jobs"]) == 3 and s["jobs"][-1]["result"] is not None)
        assert "AlreadyPresent" in state["jobs"][-1]["result"]
        assert (downloads / "file.bin").read_bytes() == b"keep existing output"
        assert native({"action": "add", "url": "https://u:secret@example.com/file"})["code"] == "invalid"
        assert native({"action": "shutdown"})["code"] == "invalid"
        assert native({"action": "add", "url": url, "directory": "/etc"})["code"] == "invalid"
        assert len(ipc({"command": "list"})["jobs"]) == 3
        ipc({"command": "shutdown", "data": {"instance": state["instance"]}})
        deadline = time.monotonic()+10
        while socket_path.exists() and time.monotonic() < deadline:
            time.sleep(.05)
        assert not socket_path.exists()
        assert native({"action": "status"})["code"] == "unavailable"
        assert not socket_path.exists(), "connection checks must not restart service"
        print("PASS real host registration/framing, on-demand service, queued feedback/removal, continued transfer, exact payload, existing-output protection, restricted commands, shutdown and read-only status")
    finally:
        if socket_path.exists():
            try:
                state = ipc({"command": "list"})
                ipc({"command": "shutdown", "data": {"instance": state["instance"]}})
                deadline = time.monotonic()+10
                while socket_path.exists() and time.monotonic() < deadline:
                    time.sleep(.05)
            except (OSError, EOFError):
                pass
        server.shutdown()
        server.server_close()
