#!/usr/bin/env python3
"""Real CLI removal: durable forgetting, no resurrection, no file deletion.
Isolated HOME/config/runtime/state; local HTTP only. Pass matching binary dir.
"""
import http.server
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time

BIN = Path(sys.argv[1]).resolve()
PAYLOAD = bytes(range(256)) * 8192
GATE = threading.Event()
REQUESTS = []


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split('?')[0]
        REQUESTS.append(path)
        if path == '/fail.bin':
            self.send_error(404)
            return
        body = PAYLOAD[:16384] if path == '/small.bin' else PAYLOAD
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            for pos in range(0, len(body), 8192):
                self.wfile.write(body[pos:pos+8192])
                self.wfile.flush()
                if path == '/blocker.bin':
                    assert GATE.wait(20), 'test did not release blocker'
                time.sleep(.008)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *args):
        pass


def receive(s, n):
    data = b''
    while len(data) < n:
        chunk = s.recv(n-len(data))
        if not chunk:
            raise EOFError('short IPC frame')
        data += chunk
    return data


with tempfile.TemporaryDirectory(prefix='rdm-remove-') as name:
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
    sock = runtime/'rdm/service.sock'
    service = None
    attached = None

    def ipc(command):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10)
            s.connect(str(sock))
            data = json.dumps({'version': 7, 'body': command}).encode()
            s.sendall(struct.pack('>I', len(data))+data)
            size = struct.unpack('>I', receive(s, 4))[0]
            response = json.loads(receive(s, size))['body']
            assert response['type'] == 'snapshot', response
            assert 'remove-secret' not in json.dumps(response)
            return response['data']

    def state():
        return ipc({'command': 'list'})

    def cli(*args, ok=True):
        p = subprocess.run([str(BIN/'rdm'), *args], env=env, capture_output=True, text=True, timeout=20)
        assert (p.returncode == 0) == ok, (args, p.stdout, p.stderr)
        assert 'remove-secret' not in p.stdout+p.stderr
        return p

    def until(predicate):
        deadline = time.monotonic()+20
        while time.monotonic() < deadline:
            try:
                s = state()
                assert sum(j['result'] is None and not j['queued'] for j in s['jobs']) <= 1
                if predicate(s):
                    return s
            except (OSError, EOFError):
                pass
            time.sleep(.025)
        raise AssertionError('expected removal state not reached')

    def job(handle):
        return next(j for j in state()['jobs'] if j['id'] == int(handle.rsplit(':', 1)[1]))

    def wait_job(handle, predicate):
        id = int(handle.rsplit(':', 1)[1])
        return until(lambda s: any(j['id'] == id and predicate(j) for j in s['jobs']))

    def add(path, output=None):
        return cli('download', base+path+'?token=remove-secret', '--detach', '-q',
                   *(['-o', output] if output else [])).stdout.strip()

    def remove(handle):
        id = int(handle.rsplit(':', 1)[1])
        p = cli('jobs', 'remove', handle)
        assert 'files were left untouched' in p.stdout
        assert all(j['id'] != id for j in state()['jobs'])
        for command in ('inspect', 'resume', 'cancel', 'remove'):
            cli('jobs', command, handle, ok=False)

    def start():
        global service
        service = subprocess.Popen([str(BIN/'rdm-service')], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        return until(lambda s: True)

    def stop():
        ipc({'command': 'shutdown', 'data': {'instance': state()['instance']}})
        _, err = service.communicate(timeout=15)
        assert service.returncode == 0, err

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    try:
        original = start()
        blocker = add('/blocker.bin')
        wait_job(blocker, lambda j: not j['queued'] and j['result'] is None)
        cli('jobs', 'remove', blocker, ok=False)  # promotion/running guard
        attached = subprocess.Popen([str(BIN/'rdm'), 'download', base+'/waiting.bin?token=remove-secret', '-q'],
                                    env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        s = until(lambda s: len(s['jobs']) == 2)
        waiting = f"{s['instance']}:{s['jobs'][-1]['id']}"
        assert job(waiting)['queued']
        remove(waiting)
        out, err = attached.communicate(timeout=10)
        assert attached.returncode != 0 and 'removed or is no longer retained' in err
        assert 'remove-secret' not in out+err
        assert '/waiting.bin' not in REQUESTS
        assert not (downloads/'waiting.bin.part').exists()
        held = add('/held.bin')
        cli('jobs', 'pause', held)
        assert job(held)['result'] == 'Paused'
        remove(held)
        assert '/held.bin' not in REQUESTS
        assert not (downloads/'held.bin.part').exists()
        partial = add('/partial.bin')
        GATE.set()
        wait_job(partial, lambda j: j['progress']['downloaded'] >= 65536)
        cli('jobs', 'pause', partial)
        wait_job(partial, lambda j: j['result'] == 'Paused')
        # Snapshot all engine artifacts AFTER safe drain; removal must not touch them.
        artifacts = {p: p.read_bytes() for p in downloads.glob('partial.bin*') if p.is_file()}
        assert any(p.suffix == '.part' for p in artifacts)
        (downloads/'partial.bin').write_bytes(b'final sentinel')
        artifacts[downloads/'partial.bin'] = b'final sentinel'
        remove(partial)
        time.sleep(.4)
        assert all(p.read_bytes() == data for p, data in artifacts.items())
        remove(blocker)  # completed history can also be dismissed, file preserved
        assert (downloads/'blocker.bin').read_bytes() == PAYLOAD
        cancelled = add('/cancelled.bin')
        wait_job(cancelled, lambda j: j['progress']['downloaded'] >= 65536)
        cli('jobs', 'cancel', cancelled)
        wait_job(cancelled, lambda j: j['result'] == 'Cancelled')
        assert job(cancelled)['retryable'], 'Cancel must remain different from Remove'
        cancel_part = downloads/'cancelled.bin.part'
        saved = cancel_part.read_bytes()
        remove(cancelled)
        assert cancel_part.read_bytes() == saved
        failed = add('/fail.bin')
        wait_job(failed, lambda j: isinstance(j['result'], dict) and 'Failed' in j['result'])
        remove(failed)
        completed = add('/small.bin')
        wait_job(completed, lambda j: isinstance(j['result'], dict) and 'Completed' in j['result'])
        remove(completed)
        assert (downloads/'small.bin').read_bytes() == PAYLOAD[:16384]
        existing = add('/small.bin')
        wait_job(existing, lambda j: isinstance(j['result'], dict) and 'AlreadyPresent' in j['result'])
        remove(existing)
        assert (downloads/'small.bin').read_bytes() == PAYLOAD[:16384]
        paused = add('/retry.bin')
        wait_job(paused, lambda j: j['progress']['downloaded'] >= 65536)
        cli('jobs', 'pause', paused)
        wait_job(paused, lambda j: j['result'] == 'Paused')
        newer = cli('jobs', 'resume', paused).stdout.strip()
        remove(paused)  # forgetting a retired source must not affect its new attempt
        wait_job(newer, lambda j: isinstance(j['result'], dict) and 'Completed' in j['result'])
        remove(newer)
        assert (downloads/'retry.bin').read_bytes() == PAYLOAD
        assert state()['jobs'] == []
        # Live private image has neither records nor retry URL; schema stays 2.
        db = root/'state/rdm/service/jobs.sqlite3'
        with sqlite3.connect(db) as con:
            image = json.loads(con.execute('SELECT payload FROM state WHERE id=1').fetchone()[0])
            assert image['records'] == [] and image['next_id'] == 11
            assert con.execute('PRAGMA user_version').fetchone()[0] == 2
        assert b'remove-secret' not in db.read_bytes()
        # Acknowledged removals survive crash AND graceful restart; no replay.
        service.kill()
        service.communicate(timeout=5)
        before = len(REQUESTS)
        recovered = start()
        assert recovered['instance'] != original['instance'] and recovered['jobs'] == []
        time.sleep(.3)
        assert len(REQUESTS) == before
        stop()
        assert start()['jobs'] == []
        next_handle = add('/small.bin', 'next.bin')
        assert next_handle.endswith(':11'), 'removed IDs must never be recycled'
        wait_job(next_handle, lambda j: j['result'] is not None)
        stop()
        print('PASS real CLI removal, attached queued waiter exits nonzero, no queued network/files, paused/cancelled/failed/completed/existing/retried removal, unchanged partial/checkpoint/final bytes, Cancel stays retryable, live URL purge, crash/graceful restart without resurrection/replay, no ID reuse')
    finally:
        GATE.set()
        if attached and attached.poll() is None:
            attached.kill()
            attached.communicate(timeout=5)
        if service and service.poll() is None:
            service.kill()
            service.communicate(timeout=5)
        server.shutdown()
        server.server_close()
