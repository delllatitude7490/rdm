#!/usr/bin/env python3
"""Real service/CLI recovery: no automatic replay, retry/resume, private SQLite.
Only isolated local HTTP and per-child environment; no browser/system state.
"""
import http.server
import json
import os
from pathlib import Path
import re
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
REQUESTS = []
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def headers_for(self, body):
        start, end = 0, len(PAYLOAD)-1
        value = self.headers.get('Range')
        if value:
            match = re.fullmatch(r'bytes=(\d+)-(\d*)', value)
            assert match, value
            start = int(match[1]); end = int(match[2]) if match[2] else end
        REQUESTS.append((self.command, self.path, start))
        self.send_response(206 if value else 200)
        self.send_header('Content-Length', str(end-start+1))
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('ETag', '"rdm-persistence-v1"')
        if value: self.send_header('Content-Range', f'bytes {start}-{end}/{len(PAYLOAD)}')
        self.end_headers()
        if body:
            try:
                for pos in range(start, end+1, 8192):
                    self.wfile.write(PAYLOAD[pos:min(pos+8192,end+1)])
                    self.wfile.flush(); time.sleep(.008)
            except (BrokenPipeError, ConnectionResetError): pass
    def do_HEAD(self): self.headers_for(False)
    def do_GET(self): self.headers_for(True)
    def log_message(self, *args): pass

def receive(s, n):
    data = b''
    while len(data) < n:
        chunk = s.recv(n-len(data))
        if not chunk: raise EOFError('truncated IPC')
        data += chunk
    return data

with tempfile.TemporaryDirectory(prefix='rdm-persistence-') as name:
    root = Path(name)
    runtime = root/'runtime'; runtime.mkdir(mode=0o700)
    config = root/'config/rdm'; config.mkdir(parents=True)
    downloads = root/'downloads'
    (config/'config.toml').write_text(f'connections=1\ndownload_dir="{downloads}"\nmax_retries=0\nqueue_parallel=1\n')
    env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root/'config'),
               XDG_STATE_HOME=str(root/'state'), XDG_RUNTIME_DIR=str(runtime))
    for key in ('RDM_ALLOW_PRIVATE', 'RDM_ALLOW_PROXY'): env.pop(key, None)
    sock = runtime/'rdm/service.sock'
    service = None
    def ipc(command):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(10); s.connect(str(sock))
            data = json.dumps({'version':6, 'body':command}).encode()
            s.sendall(struct.pack('>I',len(data))+data)
            size = struct.unpack('>I',receive(s,4))[0]
            result = json.loads(receive(s,size))['body']
            assert result['type'] == 'snapshot', result
            assert 'secret-signature' not in json.dumps(result)
            return result['data']
    def state(): return ipc({'command':'list'})
    def until(predicate):
        deadline = time.monotonic()+20
        while time.monotonic() < deadline:
            try:
                s = state()
                if predicate(s): return s
            except (OSError, EOFError): pass
            time.sleep(.03)
        raise AssertionError('expected durable service state not reached')
    def cli(*args, ok=True):
        p = subprocess.run([str(BIN/'rdm'),*args],env=env,capture_output=True,text=True,timeout=20)
        assert (p.returncode == 0) == ok, (args,p.stdout,p.stderr)
        assert 'secret-signature' not in p.stdout+p.stderr
        return p
    def start():
        global service
        service = subprocess.Popen([str(BIN/'rdm-service')],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        return until(lambda s: True)
    def stop():
        if service and service.poll() is None:
            ipc({'command':'shutdown','data':{'instance':state()['instance']}})
            out,err = service.communicate(timeout=15)
            assert service.returncode == 0, err
    server = http.server.ThreadingHTTPServer(('127.0.0.1',0), Handler)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    try:
        original = start()
        handle = cli('download',base+'/active.bin?token=secret-signature','--allow-private','--detach','-q').stdout.strip()
        until(lambda s: s['jobs'][0]['progress']['downloaded'] >= 128*1024)
        waiting = cli('download',base+'/pending.bin','--allow-private','--detach','-q').stdout.strip()
        # Admission is already durable while no pending HTTP/file work has run.
        db = root/'state/rdm/service/jobs.sqlite3'
        with sqlite3.connect(db) as con:
            image = json.loads(con.execute('SELECT payload FROM state WHERE id=1').fetchone()[0])
        assert len(image['records']) == 2 and image['records'][1]['job']['queued']
        assert all('/pending.bin' not in path for _,path,_ in REQUESTS)
        # Wait for the ENGINE checkpoint, not just UI progress: it autosaves
        # every 500ms. A crash before a checkpoint may correctly restart at zero.
        deadline = time.monotonic()+5
        while time.monotonic() < deadline:
            try:
                meta = json.loads((downloads/'active.bin.rdm').read_text())
                if any(c['completed'] > 0 for c in meta['chunks']): break
            except (FileNotFoundError, json.JSONDecodeError): pass
            time.sleep(.02)
        else: raise AssertionError('engine did not save a usable partial checkpoint')
        service.kill(); service.communicate(timeout=5)
        # Durable state survives entirely new runtime directory (like a login).
        for p in (runtime/'rdm').iterdir(): p.unlink()
        (runtime/'rdm').rmdir()
        recovered = start()
        assert recovered['instance'] != original['instance']
        assert [j['id'] for j in recovered['jobs']] == [1,2]
        assert all(j['result'] == 'Interrupted' and j['retryable'] for j in recovered['jobs'])
        count = len(REQUESTS); time.sleep(.4)
        assert len(REQUESTS) == count, 'recovery must not issue even HEAD'
        assert not (downloads/'pending.bin.part').exists()
        cli('jobs','retry',handle,ok=False)  # old instance handle must fail
        current = f"{recovered['instance']}:1"
        retried = cli('jobs','resume',current).stdout.strip()
        assert retried != current and retried.endswith(':3'), 'retry must create a safe distinct attempt'
        cli('jobs','retry',current,ok=False)  # no duplicate concurrent retry
        done = until(lambda s: any(j['id']==3 and isinstance(j['result'],dict) and 'Completed' in j['result'] for j in s['jobs']))
        assert (downloads/'active.bin').read_bytes() == PAYLOAD
        assert any(method=='GET' and offset>0 for method,path,offset in REQUESTS), 'valid engine partial must range-resume'
        assert next(j for j in done['jobs'] if j['id']==2)['result'] == 'Interrupted'
        assert not next(j for j in done['jobs'] if j['id']==1)['retryable']
        # Current policy is revalidated; original explicit CLI opt-in is retained.
        fresh_waiting = f"{done['instance']}:2"
        cli('jobs','retry',fresh_waiting)
        until(lambda s: len(s['jobs'])==4 and all(j['result'] is not None for j in s['jobs']))
        assert (downloads/'pending.bin').read_bytes() == PAYLOAD
        # Completed file is never forcibly overwritten by another submission.
        (downloads/'active.bin').write_bytes(b'keep unchanged')
        cli('download',base+'/active.bin','--allow-private','--detach','-q')
        end = until(lambda s: len(s['jobs'])==5 and s['jobs'][-1]['result'] is not None)
        assert 'AlreadyPresent' in end['jobs'][-1]['result']
        assert (downloads/'active.bin').read_bytes() == b'keep unchanged'
        stop()
        # Success purges signed URL from logical data AND database free pages.
        assert b'secret-signature' not in db.read_bytes()
        again = start()
        assert len(again['jobs']) == 5 and all(j['result'] is not None for j in again['jobs'])
        assert all(not j['retryable'] for j in again['jobs'])
        assert db.stat().st_mode & 0o777 == 0o600
        assert db.parent.stat().st_mode & 0o777 == 0o700
        # Cancel drains ALL range/autosave helpers before exposing terminal
        # state. Old checkpoint tasks must not race a later retry of this file.
        cancel_handle = cli('download',base+'/cancel.bin','--allow-private','--detach','-q').stdout.strip()
        until(lambda s: s['jobs'][-1]['progress']['downloaded'] >= 128*1024)
        cli('jobs','cancel',cancel_handle)
        until(lambda s: s['jobs'][-1]['result'] == 'Cancelled')
        meta_path = downloads/'cancel.bin.rdm'
        modified = meta_path.stat().st_mtime_ns
        time.sleep(.8)
        assert meta_path.stat().st_mtime_ns == modified, 'old autosave helper survived cancellation'
        retry_handle = cli('jobs','retry',cancel_handle).stdout.strip()
        assert retry_handle != cancel_handle
        until(lambda s: isinstance(s['jobs'][-1]['result'],dict) and 'Completed' in s['jobs'][-1]['result'])
        assert (downloads/'cancel.bin').read_bytes() == PAYLOAD
        stop()
        print('PASS SQLite admission before ACK, crash/runtime recreation, no replay, held recovery, stale handles, distinct-attempt explicit resume, duplicate rejection, real Range resume and exact bytes, retained per-job opt-in, completed history, collision protection, cancel/helper drain before retry and private URL purge')
    finally:
        if service and service.poll() is None:
            service.kill(); service.communicate(timeout=5)
        server.shutdown(); server.server_close()
