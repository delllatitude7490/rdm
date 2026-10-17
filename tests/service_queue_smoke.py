#!/usr/bin/env python3
"""CLI + GUI-style IPC + actual Native Messaging host share one FIFO.
HTTP is gated so queued requests must not inspect a server or touch outputs.
Pass directory containing matching rdm, rdm-service and rdm-native-host.
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
PAYLOAD = b'rdm-fifo\n' * 16384
GATE = threading.Event()
REQUESTS = []
class Handler(http.server.BaseHTTPRequestHandler):
    def do_HEAD(self):
        REQUESTS.append(('HEAD', self.path))
        self.send_response(200); self.send_header('Content-Length', str(len(PAYLOAD))); self.end_headers()
    def do_GET(self):
        REQUESTS.append(('GET', self.path))
        self.send_response(200); self.send_header('Content-Length', str(len(PAYLOAD))); self.end_headers()
        assert GATE.wait(15), 'test failed to release its transfer gate'
        try: self.wfile.write(PAYLOAD)
        except (BrokenPipeError, ConnectionResetError): pass
    def log_message(self, *args): pass

def receive(s, n):
    data = b''
    while len(data) < n:
        chunk = s.recv(n-len(data))
        if not chunk: raise EOFError('short IPC frame')
        data += chunk
    return data

with tempfile.TemporaryDirectory(prefix='rdm-shared-queue-') as name:
    root = Path(name)
    runtime = root / 'runtime'; runtime.mkdir(mode=0o700)
    config = root / 'config/rdm'; config.mkdir(parents=True)
    downloads = root / 'downloads'
    (config / 'config.toml').write_text(f'connections=1\ndownload_dir="{downloads}"\nmax_retries=0\nqueue_parallel=1\n')
    env = dict(os.environ, HOME=str(root), XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / 'config'), XDG_RUNTIME_DIR=str(runtime), RDM_ALLOW_PRIVATE='1')
    env.pop('RDM_ALLOW_PROXY', None)
    children = []
    def cli(*args, ok=True):
        p = subprocess.run([str(BIN/'rdm'), *args], env=env, capture_output=True, text=True, timeout=20)
        assert (p.returncode == 0) == ok, (args, p.stdout, p.stderr)
        return p
    def ipc(command):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(8); s.connect(str(runtime/'rdm/service.sock'))
            data = json.dumps({'version': 4, 'body': command}).encode()
            s.sendall(struct.pack('>I', len(data))+data)
            size = struct.unpack('>I', receive(s, 4))[0]
            response = json.loads(receive(s, size))['body']
            assert response['type'] == 'snapshot', response
            return response['data']
    def state(): return json.loads(cli('jobs', 'list', '--json').stdout)
    def until(predicate):
        deadline = time.monotonic()+15
        while time.monotonic() < deadline:
            s = state()
            assert sum(j['result'] is None and not j['queued'] for j in s['jobs']) <= 1
            if predicate(s): return s
            time.sleep(.025)
        raise AssertionError('queue state not reached')
    def native(command):
        data = json.dumps({'version': 1, 'command': command}).encode()
        p = subprocess.run([str(BIN/'rdm-native-host'), str(root/'.mozilla/native-messaging-hosts/rdm_firefox.json'), 'rdm@delllatitude7490'], input=struct.pack('=I',len(data))+data, capture_output=True, env=env, timeout=15, check=True)
        assert not p.stderr
        size = struct.unpack('=I', p.stdout[:4])[0]
        assert size == len(p.stdout)-4
        return json.loads(p.stdout[4:])
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    try:
        subprocess.run([str(BIN/'rdm-native-host'),'--install'], env=env, capture_output=True, check=True)
        first_handle = cli('download',base+'/first.bin','--detach','-q').stdout.strip()
        s = ipc({'command':'add','data':{'url':base+'/gui.bin','directory':str(downloads),'filename':'gui.bin'}})
        assert s['jobs'][1]['queued']
        native_reply = native({'action':'add','url':base+'/native.bin'})
        assert native_reply['code'] == 'queued' and native_reply['ok'], native_reply
        waiter = subprocess.Popen([str(BIN/'rdm'),'download',base+'/waiter.bin','-q'], env=env, stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        children.append(waiter)
        s = until(lambda s: len(s['jobs']) == 4)
        assert [j['label'] for j in s['jobs']] == ['first.bin','gui.bin','native.bin','waiter.bin']
        assert [j['queued'] for j in s['jobs']] == [False,True,True,True]
        assert all(j['progress']['downloaded'] == 0 for j in s['jobs'][1:])
        assert waiter.poll() is None, 'default CLI must wait while queued'
        assert all(path == '/first.bin' for _,path in REQUESTS), REQUESTS
        assert not (downloads/'gui.bin.part').exists() and not (downloads/'native.bin.part').exists()
        gui_handle = f"{s['instance']}:{s['jobs'][1]['id']}"
        cli('jobs','remove',gui_handle)
        waiter.send_signal(signal.SIGINT)
        out,err = waiter.communicate(timeout=10)
        assert waiter.returncode != 0 and 'cancelled' in err.lower(), (out,err)
        s = state()
        assert s['jobs'][0]['result'] is None, 'queued cancellation must not cancel running job'
        assert s['jobs'][1]['result'] == s['jobs'][3]['result'] == 'Cancelled'
        GATE.set()
        done = until(lambda s: s['jobs'][2]['result'] is not None)
        assert 'Completed' in done['jobs'][0]['result'] and 'Completed' in done['jobs'][2]['result'], done
        assert (downloads/'first.bin').read_bytes() == (downloads/'native.bin').read_bytes() == PAYLOAD
        assert all(path not in ('/gui.bin','/waiter.bin') for _,path in REQUESTS)
        # Attached CLI can queue and then complete normally after a predecessor.
        GATE.clear()
        cli('download',base+'/blocker.bin','--detach','-q')
        waiting = subprocess.Popen([str(BIN/'rdm'),'download',base+'/attached.bin','-q'], env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        children.append(waiting)
        until(lambda s: len(s['jobs']) == 6 and s['jobs'][-1]['queued'])
        GATE.set()
        out,err = waiting.communicate(timeout=15)
        assert waiting.returncode == 0, (out,err)
        assert (downloads/'attached.bin').read_bytes() == PAYLOAD
        # Graceful shutdown does not execute pending jobs on the way out.
        GATE.clear()
        cli('download',base+'/shutdown-active.bin','--detach','-q')
        queued = cli('download',base+'/shutdown-queued.bin','--detach','-q').stdout.strip()
        s = state()
        stopping = ipc({'command':'shutdown','data':{'instance':s['instance']}})
        assert stopping['shutting_down'] and not any(j['queued'] for j in stopping['jobs'])
        assert stopping['jobs'][-1]['result'] == 'Cancelled'
        GATE.set()
        deadline = time.monotonic()+15
        while (runtime/'rdm/service.sock').exists() and time.monotonic()<deadline: time.sleep(.03)
        assert not (runtime/'rdm/service.sock').exists()
        assert not (downloads/'shutdown-queued.bin.part').exists()
        assert all(path != '/shutdown-queued.bin' for _,path in REQUESTS)
        print('PASS shared CLI/GUI/native FIFO, queued acceptance, no queued network/files, safe remove, queued Ctrl+C, attached completion, single worker and shutdown discard')
    finally:
        GATE.set()
        for p in children:
            if p.poll() is None: p.kill(); p.wait(timeout=5)
        if (runtime/'rdm/service.sock').exists():
            s = state(); ipc({'command':'shutdown','data':{'instance':s['instance']}})
            deadline = time.monotonic()+15
            while (runtime/'rdm/service.sock').exists() and time.monotonic()<deadline: time.sleep(.03)
        server.shutdown(); server.server_close()
