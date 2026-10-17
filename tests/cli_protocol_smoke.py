#!/usr/bin/env python3
"""Bounded fake-peer fault injection for CLI ambiguity/version handling.
No network downloads; only Unix IPC. Pass directory containing built rdm.
"""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading

BIN = Path(sys.argv[1]).resolve()
def receive(s, size):
    data = b''
    while len(data) < size:
        chunk = s.recv(size - len(data))
        if not chunk: raise EOFError('short frame')
        data += chunk
    return data

for mode in ('version', 'lost-add', 'lost-feed', 'lost-cancel', 'unsafe-error', 'missing-service'):
    with tempfile.TemporaryDirectory(prefix='rdm-cli-protocol-') as name:
        root = Path(name)
        runtime = root / 'runtime'
        runtime.mkdir(mode=0o700)
        directory = runtime / 'rdm'
        directory.mkdir(mode=0o700)
        config = root / 'config/rdm'
        config.mkdir(parents=True)
        (config / 'config.toml').write_text(f'connections=1\ndownload_dir="{root}"\nmax_retries=0\nqueue_parallel=1\n')
        env = dict(os.environ, HOME=str(root), XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / 'config'), XDG_RUNTIME_DIR=str(runtime), PATH='/nonexistent')
        env.pop('RDM_ALLOW_PRIVATE', None)
        seen, errors = [], []
        server = None
        thread = None
        if mode != 'missing-service':
            server = socket.socket(socket.AF_UNIX)
            path = directory / 'service.sock'
            server.bind(str(path)); path.chmod(0o600); server.listen(4); server.settimeout(10)
            snapshot = {'instance': 'fake-1', 'revision': 1, 'default_directory': str(root), 'shutting_down': False, 'jobs': []}
            job = {'id': 1, 'label': 'file.bin', 'destination': str(root / 'file.bin'), 'progress': {'total': 100, 'downloaded': 0, 'phase': 'Downloading', 'has_notice': False}, 'queued': False, 'cancelling': False, 'result': None}
            def peer():
                try:
                    count = {'version': 1, 'lost-add': 2, 'lost-feed': 3, 'lost-cancel': 1, 'unsafe-error': 2}[mode]
                    for _ in range(count):
                        stream, _ = server.accept()
                        with stream:
                            stream.settimeout(5)
                            size = struct.unpack('>I', receive(stream, 4))[0]
                            packet = json.loads(receive(stream, size))
                            command = packet['body']['command']; seen.append(command)
                            if command == 'add_cli': snapshot['jobs'] = [job]
                            if (mode == 'lost-add' and command == 'add_cli') or (mode == 'lost-cancel' and command == 'cancel'): continue
                            if mode == 'unsafe-error' and command == 'add_cli': job['result'] = {'Failed': '\x1b[2Junsafe\x07error'}
                            data = json.dumps({'version': 1 if mode == 'version' else 6, 'body': {'type': 'snapshot', 'data': snapshot}}).encode()
                            stream.sendall(struct.pack('>I', len(data)) + data)
                    server.settimeout(.3)
                    try:
                        unexpected, _ = server.accept(); unexpected.close()
                        raise AssertionError('CLI retried after an ambiguous command or disconnect')
                    except socket.timeout: pass
                except Exception as error: errors.append(error)
            thread = threading.Thread(target=peer, daemon=True); thread.start()
        executable = BIN / 'rdm'
        if mode == 'missing-service':
            # Same filesystem hardlink; no matching sibling and empty PATH.
            executable = root / 'rdm'; os.link(BIN / 'rdm', executable)
        args = ['jobs', 'cancel', 'fake-1:1'] if mode == 'lost-cancel' else ['download', 'https://example.com/file.bin?token=never-log-this', '-q']
        try:
            p = subprocess.run([str(executable), *args], env=env, cwd=root, text=True, capture_output=True, timeout=15)
            assert p.returncode != 0, (mode, p.stdout, p.stderr)
            assert not (root / 'file.bin').exists(), 'must not silently download outside service'
            assert 'never-log-this' not in p.stdout + p.stderr
            assert '\x1b' not in p.stderr and '\x07' not in p.stderr
            if mode == 'version': assert seen == ['list'] and 'incompatible' in p.stderr
            if mode == 'lost-add': assert 'could not be confirmed' in p.stderr
            if mode == 'lost-feed': assert 'fake-1:1' in p.stderr and 'unknown' in p.stderr
            if mode == 'lost-cancel': assert seen == ['cancel'] and 'could not be confirmed' in p.stderr
            if mode == 'missing-service': assert 'Could not start rdm-service' in p.stderr
            if thread:
                thread.join(timeout=12)
                assert not thread.is_alive() and not errors, (mode, errors)
                assert seen.count('add_cli') <= 1
        finally:
            if server: server.close()
print('PASS incompatible/missing service, unconfirmed Add/Cancel, lost subscription, no mutation retries/fallback, safe terminal errors')
