#!/usr/bin/env python3
"""Real CLI/service/native-host integration; isolated env and loopback HTTP only.
Build: cargo build -p rdm -p rdm-service -p rdm-native-host
Run: python3 tests/cli_service_smoke.py target/debug
"""
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

BIN = Path(sys.argv[1]).resolve()
PAYLOAD = b'rdm-cli-service\n' * 8192
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_HEAD(self):
        self.send_response(200)
        self.send_header('Content-Length', str(len(PAYLOAD)))
        self.end_headers()
    def do_GET(self):
        self.do_HEAD()
        try:
            for offset in range(0, len(PAYLOAD), 1024):
                self.wfile.write(PAYLOAD[offset:offset + 1024])
                self.wfile.flush()
                time.sleep(.015)
        except (BrokenPipeError, ConnectionResetError):
            pass
    def log_message(self, *args): pass

def receive(stream, size):
    data = b''
    while len(data) < size:
        chunk = stream.recv(size - len(data))
        if not chunk: raise EOFError('truncated IPC')
        data += chunk
    return data

with tempfile.TemporaryDirectory(prefix='rdm-cli-test-') as name:
    root = Path(name)
    runtime = root / 'runtime'
    runtime.mkdir(mode=0o700)
    config = root / 'config/rdm'
    config.mkdir(parents=True)
    config_file = config / 'config.toml'
    # A relative configured directory MUST resolve in the CLI's cwd, not the daemon's.
    text = 'connections = 1\ndownload_dir = "downloads"\nmax_retries = 0\nqueue_parallel = 1\n'
    config_file.write_text(text)
    env = dict(os.environ, HOME=str(root), XDG_RUNTIME_DIR=str(runtime), XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / 'config'))
    for key in ('RDM_ALLOW_PRIVATE', 'RDM_ALLOW_PROXY', 'RDM_MAX_FILE_BYTES'): env.pop(key, None)
    sock = runtime / 'rdm/service.sock'
    children = []
    def cli(*args, ok=True, extra=None):
        p = subprocess.run([str(BIN / 'rdm'), *args], capture_output=True, text=True, env=env if extra is None else dict(env, **extra), cwd=root, timeout=20)
        assert (p.returncode == 0) == ok, (args, p.returncode, p.stdout, p.stderr)
        assert 'token=test-secret' not in p.stdout + p.stderr
        return p
    def ipc(command):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(8)
            stream.connect(str(sock))
            data = json.dumps({'version': 5, 'body': command}).encode()
            stream.sendall(struct.pack('>I', len(data)) + data)
            size = struct.unpack('>I', receive(stream, 4))[0]
            return json.loads(receive(stream, size))['body']
    def state(): return json.loads(cli('jobs', 'list', '--json').stdout)
    def until(predicate):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            s = state()
            if predicate(s): return s
            time.sleep(.03)
        raise AssertionError('expected service state not reached')
    def start_cli(*args):
        p = subprocess.Popen([str(BIN / 'rdm'), *args], env=env, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        children.append(p)
        return p
    def shutdown():
        if sock.exists():
            s = state()
            ipc({'command': 'shutdown', 'data': {'instance': s['instance']}})
            deadline = time.monotonic() + 12
            while sock.exists() and time.monotonic() < deadline: time.sleep(.03)
            assert not sock.exists()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f'http://127.0.0.1:{server.server_port}/file.bin?token=test-secret'
    try:
        cli('jobs', 'list', ok=False)
        assert not sock.exists(), 'inspection must not autostart'
        # Flag rejection before legacy workflows can perform network/file work.
        for value in ('sftp://example.com/file', 'https://mega.nz/file/example#key', 'https://example.com/list/'):
            cli('download', value, '--detach', ok=False)
        # First request starts matching daemon, but daemon has NO private opt-in.
        cli('download', url, '-q', '-o', 'denied.bin', ok=False)
        assert not state()['jobs']
        # Caller opts are transmitted to an already-running daemon.
        start = time.monotonic()
        cli(url, '--allow-private', '-q', '-c', '2', '-o', 'wait.bin')
        assert time.monotonic() - start > .5, 'default invocation must wait'
        assert (root / 'downloads/wait.bin').read_bytes() == PAYLOAD
        assert 'Completed' in state()['jobs'][-1]['result']
        # Completed file is not overwritten and not claimed as success.
        (root / 'downloads/wait.bin').write_bytes(b'keep')
        cli('download', url, '--allow-private', '-q', '-o', 'wait.bin', ok=False)
        assert (root / 'downloads/wait.bin').read_bytes() == b'keep'
        p = cli('download', url, '--allow-private', '--detach', '-q', '-o', 'detached.bin')
        handle = p.stdout.strip()
        assert ':' in handle and not p.stderr
        s = until(lambda s: s['jobs'][-1]['progress']['downloaded'] > 0)
        assert s['jobs'][-1]['result'] is None
        # Explicit per-job private permission does not leak into browser Add.
        before = len(s['jobs'])
        gui = ipc({'command': 'add', 'data': {'url': url, 'directory': str(root), 'filename': 'gui.bin'}})
        assert gui['type'] == 'error' and gui['data']['code'] == 'Invalid'
        queued_handle = cli('download', url, '--allow-private', '--detach', '-q', '-o', 'queued.bin').stdout.strip()
        queued = json.loads(cli('jobs', 'inspect', queued_handle, '--json').stdout)
        assert queued['queued'] and queued['progress']['downloaded'] == 0
        assert not (root / 'downloads/queued.bin.part').exists()
        cli('jobs', 'remove', handle, ok=False)  # never remove a running job
        cli('jobs', 'remove', queued_handle)
        assert json.loads(cli('jobs', 'inspect', queued_handle, '--json').stdout)['result'] == 'Cancelled'
        assert json.loads(cli('jobs', 'inspect', handle, '--json').stdout)['id'] == s['jobs'][-1]['id']
        cli('jobs', 'cancel', handle)
        until(lambda s: s['jobs'][-2]['result'] == 'Cancelled')
        # Retry the same cancelled URL/output; partial state remains engine-owned.
        cli('download', url, '--allow-private', '-q', '-o', 'detached.bin')
        assert (root / 'downloads/detached.bin').read_bytes() == PAYLOAD
        before = len(state()['jobs'])
        # Ctrl+C only cancels the waiter's job, does not shut down the service.
        p = start_cli('download', url, '--allow-private', '-q', '-o', 'interrupt.bin')
        until(lambda s: len(s['jobs']) == before + 1 and s['jobs'][-1]['progress']['downloaded'] > 0)
        p.send_signal(signal.SIGINT)
        out, err = p.communicate(timeout=15)
        assert p.returncode != 0 and 'cancelled' in err.lower(), (out, err)
        assert state()['jobs'][-1]['result'] == 'Cancelled'
        p = start_cli('download', url, '--allow-private', '-q', '-o', 'terminate.bin')
        until(lambda s: s['jobs'][-1]['label'] == 'terminate.bin' and s['jobs'][-1]['progress']['downloaded'] > 0)
        p.send_signal(signal.SIGTERM)
        out, err = p.communicate(timeout=15)
        assert p.returncode != 0 and 'cancelled' in err.lower(), (out, err)
        assert state()['jobs'][-1]['result'] == 'Cancelled'
        # Caller environment opt-in is captured even when service already runs.
        cli('download', url, '-q', '-o', 'environment.bin', extra={'RDM_ALLOW_PRIVATE': '1'})
        assert (root / 'downloads/environment.bin').read_bytes() == PAYLOAD
        # Jobs are independent of malformed local config.
        config_file.write_text('invalid toml [')
        cli('jobs', 'list', '--json')
        cli('jobs', 'inspect', handle, '--json')
        config_file.write_text(text)
        # Broken client subscription must not claim completion or cancel daemon work.
        p = start_cli('download', url, '--allow-private', '-q', '-o', 'killed-client.bin')
        until(lambda s: s['jobs'][-1]['label'] == 'killed-client.bin' and s['jobs'][-1]['progress']['downloaded'] > 0)
        p.kill(); p.communicate(timeout=5)
        until(lambda s: 'Completed' in (s['jobs'][-1]['result'] or {}))
        assert (root / 'downloads/killed-client.bin').read_bytes() == PAYLOAD
        # Restart: old handle may not cancel a new generation's ID.
        shutdown()
        cli('jobs', 'list', ok=False)
        cli('download', url, '--allow-private', '--detach', '-q', '-o', 'new-instance.bin')
        cli('jobs', 'cancel', handle, ok=False)
        assert state()['jobs'][-1]['result'] is None
        shutdown()
        print('PASS CLI wait/detach, exact bytes, relative output, private opts, queued admission/removal, inspect/cancel, SIGINT/SIGTERM drain, cancelled retry, client exit, config-independent jobs, stale handles and no-autostart queries')
    finally:
        for child in children:
            if child.poll() is None: child.kill(); child.wait(timeout=5)
        shutdown()
        server.shutdown(); server.server_close()
