import concurrent.futures
import json
import os
import pathlib
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = ROOT / 'target/debug/panasaurus-wings'


def varint(value):
    value &= 0xffffffff
    result = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        result.append(byte | (128 if value else 0))
        if not value:
            return bytes(result)


def exact(stream, count):
    result = b''
    while len(result) < count:
        chunk = stream.recv(count - len(result))
        if not chunk:
            raise EOFError('Connection closed')
        result += chunk
    return result


def readint(stream):
    result = 0
    for shift in range(0, 35, 7):
        byte = exact(stream, 1)[0]
        result |= (byte & 127) << shift
        if byte < 128:
            return result
    raise ValueError('Invalid VarInt')


def frame(stream):
    length = readint(stream)
    assert 0 < length <= 65536
    return exact(stream, length)


def send(stream, data):
    stream.sendall(varint(len(data)) + data)


def hello(port, state):
    return b'\x00' + varint(767) + b'\x09localhost' + port.to_bytes(2, 'big') + varint(state)


def free_port():
    with socket.socket() as stream:
        stream.bind(('127.0.0.1', 0))
        return stream.getsockname()[1]


def wait(check, limit=12):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.1)
    raise AssertionError('Timed out waiting for fixture condition')


class Fixture:
    def __init__(self, work, disk_mb=32):
        self.work = work
        self.server_id = str(uuid.uuid4())
        self.node_id = str(uuid.uuid4())
        self.egg_id = str(uuid.uuid4())
        self.secret = secrets.token_hex(32)
        self.public_port, self.backend_port, self.api_port = free_port(), free_port(), free_port()
        self.settings = {'uuid': self.server_id, 'suspended': False, 'egg': {'id': self.egg_id, 'file_denylist': []},
                         'build': {'disk_space': disk_mb, 'memory_limit': 128, 'cpu_limit': 25},
                         'environment': {}, 'container': {'image': 'fixture-minecraft'},
                         'allocations': {'default': {'ip': '127.0.0.1', 'port': self.public_port}, 'mappings': {'127.0.0.1': [self.public_port]}}}
        self.work.joinpath('running').write_text('false')
        self.work.joinpath('exists').write_text('true')
        self.work.joinpath('players').write_text('0')
        self.log_path = self.work / 'docker.jsonl'
        self.volume = self.work / 'volumes' / self.server_id
        self.volume.mkdir(parents=True)
        image_dir = self.work / 'volumes/.quota-images'
        image_dir.mkdir()
        self.mounted = False
        if sys.platform != 'darwin':
            raise RuntimeError('This fixture requires macOS quota images; run it on the local development host')
        image = image_dir / (self.server_id + '.sparseimage')
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', str(disk_mb) + 'm', '-type', 'SPARSE', '-fs', 'HFS+', '-volname', 'Panasaurus-hibernation-test', str(image)], check=True, capture_output=True)
        subprocess.run(['/usr/bin/hdiutil', 'attach', '-nobrowse', '-owners', 'on', '-mountpoint', str(self.volume), str(image)], check=True, capture_output=True)
        self.mounted = True
        (self.volume / '.server-data').mkdir()
        (image_dir / (self.server_id + '.ready')).write_text(str(disk_mb * 1024 * 1024))
        owner = self

        class Panel(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.startswith('/api/remote/servers?'):
                    data = json.dumps({'data': [{'uuid': owner.server_id, 'settings': owner.settings}], 'meta': {'last_page': 1}}).encode()
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.end_headers()
                    self.wfile.write(data)
                else:
                    self.send_error(404)

            def log_message(self, *args):
                pass

        self.panel = ThreadingHTTPServer(('127.0.0.1', 0), Panel)
        threading.Thread(target=self.panel.serve_forever, daemon=True).start()
        executable = self.work / 'docker-fixture.py'
        executable.write_text('''#!/usr/bin/env python3
import json,pathlib,sys,time
work=pathlib.Path(__file__).parent
args=sys.argv[1:]
with (work/'docker.jsonl').open('a') as log:log.write(json.dumps(args)+'\\n')
if args[0]=='inspect':
 if (work/'exists').read_text()!='true':print('No such container',file=sys.stderr);sys.exit(1)
 print(json.dumps([{'Config':{'Labels':{'dev.panasaurus.managed':'true','dev.panasaurus.node':NODE_ID,'dev.panasaurus.hibernation':LISTEN}},'HostConfig':{'ReadonlyRootfs':True,'PortBindings':{KEY:[{'HostIp':'127.0.0.1','HostPort':str(BACKEND)}]}},'NetworkSettings':{'Ports':{KEY:[{'HostIp':'127.0.0.1','HostPort':str(BACKEND)}]}},'State':{'Running':(work/'running').read_text()=='true','StartedAt':'2026-10-11T00:00:00Z'}}]))
elif args[0]=='stats':print(json.dumps({'CPUPerc':'0%','MemUsage':'0B / 128MiB','NetIO':'0B / 0B'}))
elif args[0]=='start':
 if (work/'fail-start').exists():print('Fixture start failure',file=sys.stderr);sys.exit(1)
 (work/'running').write_text('true')
elif args[0] in ['stop','kill','rm']:
 (work/'running').write_text('false')
 if args[0]=='rm':(work/'exists').write_text('false')
elif args[0]=='attach':
 command=sys.stdin.buffer.readline()
 with (work/'commands.txt').open('ab') as log:log.write(command)
 if command==b'stop\\n' and not (work/'ignore-stop').exists():(work/'running').write_text('false')
 sys.stdin.buffer.read(2)
elif args[0]=='logs':pass
else:print('{}')
'''.replace('NODE_ID', repr(self.node_id)).replace('LISTEN', repr(f'127.0.0.1:{self.public_port}')).replace('KEY', repr(f'{self.public_port}/tcp')).replace('BACKEND', str(self.backend_port)))
        executable.chmod(0o700)
        self.backend = socket.socket()
        self.backend.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.backend.bind(('127.0.0.1', self.backend_port))
        self.backend.listen()
        self.backend.settimeout(.2)
        self.closed = False
        self.forwarded = []
        threading.Thread(target=self.accept_backend, daemon=True).start()
        self.config = self.work / 'config.json'
        self.config.write_text(json.dumps({'uuid': self.node_id, 'token_id': 'fixture', 'token': self.secret,
            'remote': f'http://127.0.0.1:{self.panel.server_port}', 'jwt_audience': f'http://127.0.0.1:{self.api_port}',
            'api': {'host': '127.0.0.1', 'port': self.api_port}, 'system': {'data': str(self.work / 'volumes')},
            'docker': {'executable': str(executable), 'command_timeout_seconds': 5},
            'hibernation': [{'server': self.server_id, 'egg': self.egg_id, 'listen': f'127.0.0.1:{self.public_port}',
                'idle_timeout_seconds': 2, 'startup_grace_seconds': 1, 'poll_interval_seconds': 1,
                'startup_timeout_seconds': 5, 'stop_timeout_seconds': 3}]}))
        self.config.chmod(0o600)
        self.log = (self.work / 'daemon.log').open('ab')
        self.daemon = None
        self.start()

    def running(self):
        return (self.work / 'running').read_text() == 'true'

    def accept_backend(self):
        while not self.closed:
            try:
                stream, _ = self.backend.accept()
                threading.Thread(target=self.handle_backend, args=(stream,), daemon=True).start()
            except socket.timeout:
                pass
            except OSError:
                break

    def handle_backend(self, stream):
        try:
            with stream:
                stream.settimeout(3)
                if not self.running():
                    return
                handshake = frame(stream)
                if handshake[-1] == 1:
                    assert frame(stream) == b'\x00'
                    ping = frame(stream)
                    count = (self.work / 'players').read_text()
                    players = {'max': 20}
                    if count != 'unknown':
                        players['online'] = int(count)
                    text = json.dumps({'version': {'name': 'Fixture', 'protocol': 767}, 'players': players, 'description': {'text': 'Ready'}}).encode()
                    send(stream, b'\x00' + varint(len(text)) + text)
                    send(stream, ping)
                else:
                    login = frame(stream)
                    self.forwarded.append((handshake, login))
                    stream.sendall(b'PASSTHROUGH')
                    stream.settimeout(.2)
                    while not self.closed and self.running():
                        try:
                            data = stream.recv(4096)
                            if not data:
                                break
                            stream.sendall(data)
                        except socket.timeout:
                            pass
        except (OSError, EOFError):
            pass

    def api(self, path='', body=None, method='GET'):
        url = f'http://127.0.0.1:{self.api_port}' + (path or f'/api/servers/{self.server_id}')
        request = urllib.request.Request(url, data=json.dumps(body).encode() if body is not None else None, method=method,
            headers={'Authorization': 'Bearer ' + self.secret, 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=8) as response:
                data = response.read()
                return json.loads(data) if data else response.status
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"Fixture API {method} {path} returned {error.code}: {error.read().decode()}") from error

    def start(self):
        self.daemon = subprocess.Popen([str(BINARY), '--config', str(self.config)], stdout=self.log, stderr=subprocess.STDOUT)
        def healthy():
            if self.daemon.poll() is not None:
                raise RuntimeError((self.work / 'daemon.log').read_text())
            try:
                return self.api('/api/system') is not None
            except (OSError, urllib.error.URLError):
                return False
        wait(healthy)

    def stop_daemon(self):
        if self.daemon:
            self.daemon.terminate()
            self.daemon.wait(timeout=5)
            self.daemon = None

    def sleeping(self):
        return self.api()['utilization']['hibernation']['sleeping'] and not self.running()

    def status(self):
        with socket.create_connection(('127.0.0.1', self.public_port), timeout=6) as stream:
            send(stream, hello(self.public_port, 1))
            send(stream, b'\x00')
            send(stream, b'\x01' + b'12345678')
            packet = frame(stream)
            assert packet[0] == 0
            length_stream = MemorySocket(packet[1:])
            text = exact(length_stream, readint(length_stream))
            assert frame(stream) == b'\x01' + b'12345678'
            return json.loads(text)

    def join(self, valid=True):
        stream = socket.create_connection(('127.0.0.1', self.public_port), timeout=8)
        send(stream, hello(self.public_port, 2))
        send(stream, b'\x00\x043z4k' + uuid.UUID(self.server_id).bytes if valid else b'\x00\x03../')
        return stream

    def starts(self):
        return sum(json.loads(line)[0] == 'start' for line in self.log_path.read_text().splitlines())

    def power(self, action):
        return self.api(f'/api/servers/{self.server_id}/power', {'action': action}, 'POST')

    def close(self):
        self.closed = True
        self.stop_daemon()
        self.backend.close()
        self.panel.shutdown()
        self.panel.server_close()
        self.log.close()
        if self.mounted:
            subprocess.run(['/usr/bin/hdiutil', 'detach', str(self.volume)], check=True, capture_output=True)
            self.mounted = False


class MemorySocket:
    def __init__(self, data):
        self.data = data

    def recv(self, count):
        result, self.data = self.data[:count], self.data[count:]
        return result


def main():
    results = {}
    with tempfile.TemporaryDirectory(prefix='panasaurus-hibernation-') as temporary:
        fixture = Fixture(pathlib.Path(temporary).resolve())
        try:
            assert fixture.status()['players']['online'] == 0
            with fixture.join() as stream:
                assert stream.recv(64) == b''
            assert fixture.starts() == 0
            results['manual_offline_not_woken'] = True
            fixture.power('start')
            (fixture.work / 'players').write_text('2')
            time.sleep(4)
            assert fixture.running() and not fixture.sleeping()
            results['players_keep_server_awake'] = True
            (fixture.work / 'players').write_text('unknown')
            time.sleep(4)
            assert fixture.running()
            results['unknown_player_count_keeps_server_awake'] = True
            (fixture.work / 'players').write_text('0')
            wait(fixture.sleeping)
            assert (fixture.work / 'commands.txt').read_text() == 'stop\n'
            results['verified_empty_server_saves_and_sleeps'] = True
            before = fixture.starts()
            for _ in range(3):
                assert 'Sleeping' in fixture.status()['description']['text']
            with fixture.join(False) as stream:
                assert stream.recv(64) == b''
            assert fixture.starts() == before
            results['status_pings_and_invalid_logins_do_not_wake'] = True
            fixture.stop_daemon()
            fixture.start()
            assert fixture.sleeping()
            results['sleep_state_survives_daemon_restart'] = True
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                clients = list(pool.map(lambda _: fixture.join(), range(2)))
            try:
                for stream in clients:
                    assert exact(stream, len(b'PASSTHROUGH')) == b'PASSTHROUGH'
                    stream.sendall(b'opaque-encrypted-payload')
                    assert exact(stream, len(b'opaque-encrypted-payload')) == b'opaque-encrypted-payload'
                assert fixture.starts() == before + 1
                assert len(fixture.forwarded) == 2
                assert all(item[0] == hello(fixture.public_port, 2) for item in fixture.forwarded)
                time.sleep(4)
                assert fixture.running()
                results['concurrent_joins_wake_once_and_forward_unchanged'] = True
                results['pending_and_connected_logins_prevent_sleep'] = True
            finally:
                for stream in clients:
                    stream.close()
            wait(fixture.sleeping)
            fixture.settings['suspended'] = True
            before = fixture.starts()
            with fixture.join() as stream:
                assert stream.recv(64) == b''
            assert fixture.starts() == before and not fixture.running()
            results['fresh_panel_suspension_blocks_automatic_wake'] = True
            fixture.settings['suspended'] = False
            fixture.power('stop')
            with fixture.join() as stream:
                assert stream.recv(64) == b''
            assert fixture.starts() == before
            fixture.stop_daemon()
            fixture.start()
            with fixture.join() as stream:
                assert stream.recv(64) == b''
            assert fixture.starts() == before
            results['manual_stop_disarms_wake_across_restart'] = True
            (fixture.work / 'ignore-stop').touch()
            fixture.power('start')
            time.sleep(8)
            assert fixture.running()
            commands = [json.loads(line)[0] for line in fixture.log_path.read_text().splitlines()]
            assert 'kill' not in commands
            results['save_timeout_never_force_kills'] = True
            (fixture.work / 'ignore-stop').unlink()
            fixture.power('start')
            wait(fixture.sleeping)
            (fixture.work / 'fail-start').touch()
            before = fixture.starts()
            for _ in range(2):
                with fixture.join() as stream:
                    assert stream.recv(64) == b''
            assert fixture.starts() == before + 1
            results['failed_wake_has_retry_cooldown'] = True
            assert fixture.api(f'/api/servers/{fixture.server_id}', method='DELETE') == 204
            def listener_closed():
                try:
                    with socket.create_connection(('127.0.0.1', fixture.public_port), timeout=.2):
                        return False
                except OSError:
                    return True
            wait(listener_closed)
            results['server_deletion_closes_proxy'] = True
            fixture.stop_daemon()
            fixture.start()
            results['deleted_server_configuration_does_not_block_node_restart'] = True
        except Exception:
            evidence = ROOT / '.local/hibernation/fixture-failure.log'
            evidence.parent.mkdir(parents=True, exist_ok=True)
            evidence.write_text((fixture.work / 'daemon.log').read_text())
            raise
        finally:
            fixture.close()
    print(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
