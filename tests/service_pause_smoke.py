#!/usr/bin/env python3
"""Real service/CLI pause: drain, FIFO, persistent holds and exact-byte resume.
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
        value = None if self.path.startswith(('/no-range.bin', '/late-stream.bin', '/late-empty.bin')) else self.headers.get('Range')
        if value:
            match = re.fullmatch(r'bytes=(\d+)-(\d*)', value)
            assert match, value
            start = int(match[1]); end = int(match[2]) if match[2] else end
        REQUESTS.append((self.command, self.path, start))
        if self.path.startswith('/late-') and not (downloads/self.path.removeprefix('/')).exists():
            (downloads/self.path.removeprefix('/')).write_bytes(b'late file must survive')
        self.send_response(206 if value else 200)
        if self.path.startswith('/late-stream.bin'):
            self.send_header('Connection', 'close'); self.close_connection = True
        else:
            self.send_header('Content-Length', '0' if self.path.startswith('/late-empty.bin') else str(end-start+1))
        if not self.path.startswith('/no-range.bin'): self.send_header('Accept-Ranges', 'bytes')
        self.send_header('ETag', '"rdm-pause-v1"')
        if value: self.send_header('Content-Range', f'bytes {start}-{end}/{len(PAYLOAD)}')
        self.end_headers()
        if body and not self.path.startswith('/late-empty.bin'):
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

with tempfile.TemporaryDirectory(prefix='rdm-pause-') as name:
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
            data = json.dumps({'version':7, 'body':command}).encode()
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
    attached = None
    try:
        original = start()
        def handle(id): return f"{state()['instance']}:{id}"
        def job(id): return next(j for j in state()['jobs'] if j['id']==id)
        def wait_job(id, predicate): return until(lambda s: any(j['id']==id and predicate(j) for j in s['jobs']))
        # Attached callers must receive a truthful Paused result, never success.
        attached = subprocess.Popen([str(BIN/'rdm'),'download',base+'/range.bin?token=secret-signature',
                                   '--allow-private','-q'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        wait_job(1,lambda j: j['progress']['downloaded'] >= 128*1024)
        second = cli('download',base+'/second.bin','--allow-private','--detach','-q').stdout.strip()
        third = cli('download',base+'/held.bin','--allow-private','--detach','-q').stdout.strip()
        cli('jobs','pause',third)
        assert job(3)['result']=='Paused' and job(3)['retryable']
        assert not (downloads/'held.bin.part').exists()
        assert all('/held.bin' not in p for _,p,_ in REQUESTS)
        first = handle(1)
        cli('jobs','pause',first)
        wait_job(1,lambda j: j['result']=='Paused')
        out,err = attached.communicate(timeout=15)
        assert attached.returncode != 0 and 'paused' in err.lower() and 'secret-signature' not in out+err
        assert not job(1)['pausing'] and not job(1)['cancelling']
        # Pausing releases the one worker and advances FIFO, not held jobs.
        wait_job(2,lambda j: isinstance(j['result'],dict) and 'Completed' in j['result'])
        assert (downloads/'second.bin').read_bytes()==PAYLOAD
        assert job(3)['result']=='Paused'
        meta = downloads/'range.bin.rdm'
        stamp=meta.stat().st_mtime_ns
        count=len(REQUESTS); time.sleep(.8)
        assert meta.stat().st_mtime_ns==stamp, 'checkpoint helpers outlived pause'
        assert len(REQUESTS)==count, 'held work performed network requests'
        cli('jobs','pause',first) # idempotent, not duplicate attempt
        cli('jobs','pause',second,ok=False) # successful history cannot pause
        assert len(state()['jobs'])==3
        # A crash preserves completed Paused holds, not Interrupted or auto-play.
        service.kill(); service.communicate(timeout=5)
        recovered=start()
        assert recovered['instance'] != original['instance']
        assert job(1)['result']=='Paused' and job(3)['result']=='Paused'
        count=len(REQUESTS);time.sleep(.5)
        assert len(REQUESTS)==count
        cli('jobs','resume',first,ok=False) # obsolete service generation
        before=len(REQUESTS)
        resumed=cli('jobs','resume',handle(1)).stdout.strip()
        assert resumed.endswith(':4') and job(1)['retried_as']==4
        cli('jobs','resume',handle(1),ok=False) # no duplicate replay
        cli('jobs','cancel',handle(1)) # retired handle must not cancel job 4
        wait_job(4,lambda j: isinstance(j['result'],dict) and 'Completed' in j['result'])
        assert (downloads/'range.bin').read_bytes()==PAYLOAD
        assert any(m=='GET' and '/range.bin' in p and off>0 for m,p,off in REQUESTS[before:]), 'expected byte-range continuation'
        assert job(3)['result']=='Paused'
        cli('jobs','remove',handle(3))
        assert all(j['id'] != 3 for j in state()['jobs'])
        cli('jobs','resume',handle(3),ok=False)
        assert not (downloads/'held.bin.part').exists()
        # No-range server: never append a full response to a retained prefix.
        plain=cli('download',base+'/no-range.bin','--allow-private','--detach','-q').stdout.strip()
        wait_job(5,lambda j: j['progress']['downloaded']>=128*1024)
        cli('jobs','pause',plain)
        wait_job(5,lambda j: j['result']=='Paused')
        old_count=len(REQUESTS)
        assert cli('jobs','resume',plain).stdout.strip().endswith(':6')
        wait_job(6,lambda j: isinstance(j['result'],dict) and 'Completed' in j['result'])
        assert (downloads/'no-range.bin').read_bytes()==PAYLOAD
        assert any(m=='GET' and p.startswith('/no-range.bin') and off==0 for m,p,off in REQUESTS[old_count:])
        # Existing final output wins over a deliberately resumed partial.
        collision=cli('download',base+'/collision.bin','--allow-private','--detach','-q').stdout.strip()
        wait_job(7,lambda j: j['progress']['downloaded']>=128*1024)
        cli('jobs','pause',collision)
        wait_job(7,lambda j: j['result']=='Paused')
        (downloads/'collision.bin').write_bytes(b'keep existing output')
        assert cli('jobs','resume',collision).stdout.strip().endswith(':8')
        wait_job(8,lambda j: isinstance(j['result'],dict) and 'AlreadyPresent' in j['result'])
        assert (downloads/'collision.bin').read_bytes()==b'keep existing output'
        # A failed final checkpoint during pause is Failed, never clean Paused.
        broken=cli('download',base+'/checkpoint.bin','--allow-private','--detach','-q').stdout.strip()
        wait_job(9,lambda j: j['progress']['downloaded']>=128*1024)
        bad_meta=downloads/'checkpoint.bin.rdm'
        if bad_meta.exists(): bad_meta.rename(downloads/'checkpoint.bin.saved-meta')
        bad_meta.mkdir()
        cli('jobs','pause',broken)
        wait_job(9,lambda j: j['result'] is not None)
        assert isinstance(job(9)['result'],dict) and 'Failed' in job(9)['result'], job(9)
        assert not job(9)['pausing']
        # Final output arriving during inspection is never overwrite approval.
        # Cover ranged, streaming/unknown-size and zero-length publication.
        for id,filename in enumerate(['late-range.bin','late-stream.bin','late-empty.bin'],10):
            cli('download',base+'/'+filename,'--allow-private','--detach','-q')
            wait_job(id,lambda j: j['result'] is not None)
            assert isinstance(job(id)['result'],dict) and 'Failed' in job(id)['result'], job(id)
            assert (downloads/filename).read_bytes()==b'late file must survive'
        stop()
        db=root/'state/rdm/service/jobs.sqlite3'
        with sqlite3.connect(db) as con:
            assert con.execute('PRAGMA user_version').fetchone()[0]==2
        assert b'secret-signature' not in db.read_bytes()
        print('PASS real CLI pause/attached outcome, drained checkpoint helpers, FIFO advancement, queued no-work hold, idempotence, crash-persistent Paused/no replay, stale handles, fresh-attempt Range resume/exact bytes, no-range safe restart, existing-output protection, checkpoint-failure honesty, Remove and URL purge')
    finally:
        if attached and attached.poll() is None:
            attached.kill();attached.communicate(timeout=5)
        if service and service.poll() is None:
            service.kill();service.communicate(timeout=5)
        server.shutdown();server.server_close()
