#!/usr/bin/env python3
"""Real service bulk-clear: explicit IDs, protected work, durable no-file deletion."""
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
BODY = b'history-test\n' * 512
REQUESTS = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        REQUESTS.append(self.path)
        if self.path == '/fail.bin':
            self.send_error(404)
            return
        slow = self.path.endswith('.bin')
        body = BODY * 1024 if slow else BODY
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            for pos in range(0, len(body), 8192):
                self.wfile.write(body[pos:pos+8192])
                self.wfile.flush()
                if slow:
                    time.sleep(.012)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *args):
        pass


def receive(s, n):
    data = b''
    while len(data) < n:
        chunk = s.recv(n-len(data))
        if not chunk:
            raise EOFError()
        data += chunk
    return data


with tempfile.TemporaryDirectory(prefix='rdm-history-') as name:
    root = Path(name)
    runtime = root/'runtime'
    runtime.mkdir(mode=0o700)
    config = root/'config/rdm'
    config.mkdir(parents=True)
    downloads = root/'downloads'
    (config/'config.toml').write_text(f'connections=1\ndownload_dir="{downloads}"\nmax_retries=0\nqueue_parallel=1\n')
    env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root/'config'),
               XDG_STATE_HOME=str(root/'state'), XDG_RUNTIME_DIR=str(runtime), RDM_ALLOW_PRIVATE='1')
    env.pop('RDM_ALLOW_PROXY', None)
    service = None

    def ipc(command, ok=True):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10)
            s.connect(str(runtime/'rdm/service.sock'))
            data = json.dumps({'version': 7, 'body': command}).encode()
            s.sendall(struct.pack('>I', len(data))+data)
            size = struct.unpack('>I', receive(s, 4))[0]
            response = json.loads(receive(s, size))['body']
            assert (response['type'] == 'snapshot') == ok, response
            return response['data']

    def state():
        return ipc({'command': 'list'})

    def cli(*args):
        p = subprocess.run([str(BIN/'rdm'), *args], env=env, capture_output=True, text=True, timeout=20)
        assert p.returncode == 0, (args, p.stdout, p.stderr)
        return p.stdout.strip()

    def until(predicate):
        deadline = time.monotonic()+20
        while time.monotonic() < deadline:
            try:
                s = state()
                if predicate(s):
                    return s
            except (OSError, EOFError):
                pass
            time.sleep(.025)
        raise AssertionError('history state not reached')

    def add(path):
        handle = cli('download', base+path, '--detach', '-q')
        return handle, int(handle.rsplit(':', 1)[1])

    def wait_job(id, predicate):
        return until(lambda s: any(j['id'] == id and predicate(j) for j in s['jobs']))

    def clear(ids, ok=True, instance=None):
        return ipc({'command': 'clear_finished', 'data': {'instance': instance or state()['instance'], 'ids': ids}}, ok)

    def start():
        global service
        service = subprocess.Popen([str(BIN/'rdm-service')], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        return until(lambda s: True)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    try:
        initial = start()
        _, first = add('/first.txt')
        wait_job(first, lambda j: j['result'] is not None)
        confirmed_ids = [first]
        _, newer = add('/newer.txt')
        wait_job(newer, lambda j: j['result'] is not None)
        clear(confirmed_ids)
        assert [j['id'] for j in state()['jobs']] == [newer], 'new completions must not broaden the confirmed set'
        assert (downloads/'first.txt').read_bytes() == BODY
        _, existing = add('/newer.txt')
        wait_job(existing, lambda j: isinstance(j['result'], dict) and 'AlreadyPresent' in j['result'])
        _, failed = add('/fail.bin')
        wait_job(failed, lambda j: isinstance(j['result'], dict) and 'Failed' in j['result'])
        cancelled_handle, cancelled = add('/cancelled.bin')
        wait_job(cancelled, lambda j: j['progress']['downloaded'] > 65536)
        cli('jobs', 'cancel', cancelled_handle)
        wait_job(cancelled, lambda j: j['result'] == 'Cancelled')
        paused_handle, paused = add('/paused.bin')
        wait_job(paused, lambda j: j['progress']['downloaded'] > 65536)
        cli('jobs', 'pause', paused_handle)
        wait_job(paused, lambda j: j['result'] == 'Paused')
        _, active = add('/active.bin')
        wait_job(active, lambda j: j['progress']['downloaded'] > 65536)
        _, queued = add('/queued.bin')
        hold_handle, held = add('/held.bin')
        cli('jobs', 'pause', hold_handle)
        artifacts = {p: p.read_bytes() for p in downloads.glob('*') if p.is_file() and not p.name.startswith('active.bin')}
        expected = [j['id'] for j in state()['jobs']]
        for protected in [active, queued, paused, held, failed, cancelled]:
            error = clear([newer, protected], ok=False)
            assert error['code'] == 'Invalid'
            assert [j['id'] for j in state()['jobs']] == expected, 'batch rejection must be atomic'
        assert clear([newer], ok=False, instance='stale')['code'] == 'StaleInstance'
        clear([newer, existing])
        assert all(j['id'] not in (first, newer, existing) for j in state()['jobs'])
        assert all(p.read_bytes() == data for p, data in artifacts.items())
        assert not (downloads/'queued.bin.part').exists() and not (downloads/'held.bin.part').exists()
        assert '/queued.bin' not in REQUESTS and '/held.bin' not in REQUESTS
        # A duplicate/lost selection is a no-op, never broadens to other records.
        before = state()
        clear([newer, existing])
        assert state()['revision'] >= before['revision']  # active progress may advance independently
        assert [j['id'] for j in state()['jobs']] == [j['id'] for j in before['jobs']]
        service.kill()
        service.communicate(timeout=5)
        before_requests = len(REQUESTS)
        recovered = start()
        assert all(j['id'] not in (first, newer, existing) for j in recovered['jobs'])
        assert {j['id']: j['result'] for j in recovered['jobs'] if j['id'] in (active, queued)} == {active: 'Interrupted', queued: 'Interrupted'}
        assert all(j['retryable'] for j in recovered['jobs'])
        time.sleep(.3)
        assert len(REQUESTS) == before_requests, 'clearing/recovery must not replay downloads'
        assert (downloads/'first.txt').read_bytes() == BODY and (downloads/'newer.txt').read_bytes() == BODY
        ipc({'command': 'shutdown', 'data': {'instance': recovered['instance']}})
        _, err = service.communicate(timeout=15)
        assert service.returncode == 0, err
        print('PASS atomic explicit-ID ClearFinished, newer completions excluded, stale instance/protected batch rejection, paused/failed/cancelled/running/queued work retained, no file/checkpoint deletion, durable crash recovery with no resurrection/replay')
    finally:
        if service and service.poll() is None:
            service.kill()
            service.communicate(timeout=5)
        server.shutdown()
        server.server_close()
