#!/usr/bin/env python3
"""Exercise the deployed bridge via an isolated copy of the exact live web binary.

Safety model
------------
This script can touch the *real* desktop clipboard, so by default it refuses to
do so. Two independent gates must both be open:

  1. the explicit ``--authorize-destructive-test`` flag, and
  2. a live guard lease: the clipboard preservation guard (Guard.cs) must have
     published ``ready`` for *this* run nonce and session, no ``done`` marker may
     already exist, the guard deadline must still be in the future, and a
     verified ``restored`` result is required at the end.

The lease is re-checked immediately before every clipboard write, so a guard
that expired mid-run stops the verification instead of letting it overwrite an
already restored clipboard. A failed or unproven restoration returns a distinct
non-zero exit status instead of looking like success.

Without the flag only the non-writing checks run (authentication, Origin, host
ownership, framing limits, bridge read path).

Run on the Moonlight Web host only, with a separately staged guard directory.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent
HOST_ID = 1016252525
TEMP_CONTAINER = 'moonlight-clipboard-verify'
BASE = 'http://127.0.0.1:18081'

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_NOT_AUTHORIZED = 3
EXIT_LEASE_INVALID = 4
EXIT_RESTORE_UNVERIFIED = 5


class LeaseError(RuntimeError):
    """The clipboard preservation guard is absent, stale, expired or finished."""


def docker_json(name):
    return json.loads(subprocess.check_output(['docker', 'exec', 'moonlight-web', 'cat', '/moonlight-web/server/' + name]))


def request(base, path, method='GET', payload=None, headers=None, raw=None):
    headers = dict(headers or {})
    if raw is not None:
        data = raw
    else:
        data = None if payload is None else json.dumps(payload, ensure_ascii=False).encode()
    if data is not None:
        headers.setdefault('Content-Type', 'application/json')
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as res:
            return res.status, res.headers, res.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()


def check(label, condition):
    if not condition:
        raise AssertionError(label)
    print('PASS:', label, flush=True)


def assert_write_rejected(request, base, headers, label, payload=None, raw=None):
    """A POST the bridge must reject *before* it touches the clipboard.

    These probes deliberately send a body the bridge refuses (malformed JSON,
    NUL text, oversized text), so they cannot change the clipboard on a correct
    bridge. They assert a 4xx so a regression that started accepting them
    fails loudly instead of silently writing.
    """
    status = request(base, '/clipboard', method='POST', payload=payload, headers=headers, raw=raw)[0]
    check(label, status in (400, 413))
    return status


# --------------------------------------------------------------------------
# guard lease
# --------------------------------------------------------------------------
def read_guard(guard_dir):
    state = {'exists': Path(guard_dir).is_dir()}
    for name in ('ready', 'status', 'restored', 'done'):
        path = Path(guard_dir) / name
        if path.exists():
            try:
                state[name] = path.read_text().strip()
            except OSError as exc:
                state[name] = '<unreadable: ' + exc.__class__.__name__ + '>'
    return state


def parse_fields(text):
    fields = {}
    for token in (text or '').split():
        if '=' in token:
            key, _, value = token.partition('=')
            fields[key] = value
    return fields


class GuardLease:
    """The single authority that decides whether a clipboard write is allowed."""

    def __init__(self, guard_dir, run_nonce, session_id=None,
                 lifetime_seconds=180.0, clock=time.time):
        self.guard_dir = Path(guard_dir)
        self.run_nonce = run_nonce
        self.session_id = session_id
        self.lifetime_seconds = lifetime_seconds
        self.clock = clock
        self.acquired_at = None
        self.writes_performed = 0

    def require_ready(self):
        if not self.guard_dir.is_dir():
            raise LeaseError('guard directory ' + str(self.guard_dir) + ' does not exist')
        state = read_guard(self.guard_dir)
        if 'ready' not in state:
            raise LeaseError('guard has not published `ready`; a clipboard preservation '
                             'snapshot is not confirmed, refusing to touch the clipboard')
        if 'done' in state:
            raise LeaseError('`done` is already present: a previous verification finished, or a '
                             'stale marker was left behind; re-arm the guard')
        if 'restored' in state:
            raise LeaseError('`restored` is already present: the guard already restored the '
                             'clipboard, so this run would write after restoration')

        fields = parse_fields(state.get('ready'))
        if fields.get('run') != self.run_nonce:
            raise LeaseError('guard `ready` is for run ' + repr(fields.get('run')) +
                             ', not this run ' + repr(self.run_nonce) + '; refusing a stale marker')
        if self.session_id is not None and fields.get('session') != str(self.session_id):
            raise LeaseError('guard `ready` is for session ' + repr(fields.get('session')) +
                             ', not ' + repr(self.session_id))
        if fields.get('session') in ('', '0'):
            raise LeaseError('guard reported session 0, which cannot own an interactive clipboard')

        deadline = fields.get('deadline')
        if deadline is not None:
            try:
                remaining = float(deadline) - self.clock()
            except ValueError:
                raise LeaseError('guard `ready` deadline ' + repr(deadline) + ' is not a number') from None
            if remaining <= 0:
                raise LeaseError(f'guard lease expired {abs(remaining):.1f}s ago')

        status = state.get('status', '')
        if 'capture-failed' in status:
            raise LeaseError('guard failed to capture the clipboard: ' + repr(status))
        return state

    def acquire(self):
        state = self.require_ready()
        self.acquired_at = self.clock()
        deadline = parse_fields(state.get('ready')).get('deadline')
        if deadline is not None:
            remaining = float(deadline) - self.acquired_at
            if remaining < self.remaining_budget():
                raise LeaseError(f'guard lease expires in {remaining:.1f}s, before this '
                                 'verification can finish')
        return state

    def remaining_budget(self):
        return max(self.lifetime_seconds * 0.2, 5.0)

    def before_write(self):
        """Re-check the lease immediately before a clipboard write."""
        self.require_ready()
        if self.acquired_at is not None:
            elapsed = self.clock() - self.acquired_at
            if elapsed > self.lifetime_seconds:
                raise LeaseError(f'local budget of {self.lifetime_seconds:.0f}s exceeded after '
                                 f'{elapsed:.1f}s; the guard may already have restored')
        self.writes_performed += 1

    def release(self, restore_timeout=30.0):
        (self.guard_dir / 'done').write_text('run=' + self.run_nonce + '\n')
        deadline = self.clock() + restore_timeout
        while self.clock() < deadline:
            state = read_guard(self.guard_dir)
            if state.get('restored') is not None:
                return state
            time.sleep(0.2)
        return read_guard(self.guard_dir)

    def verify_restored(self, state):
        restored = state.get('restored')
        status = state.get('status', '')
        if restored is None:
            raise LeaseError('the guard did not report a restoration result after `done`; '
                             'treat the clipboard state as unknown')
        if restored.strip().lower() != 'true' or 'RESTORE-FAILED' in status:
            raise LeaseError('clipboard restoration was not verified (restored=' +
                             repr(restored) + ', status=' + repr(status) + ')')


# --------------------------------------------------------------------------
# isolated snapshot
# --------------------------------------------------------------------------
def make_snapshot(data, password, salt):
    """Build the isolated verification data. No real password is copied."""
    host = data['hosts'][str(HOST_ID)]
    owner = str(host['owner'])
    user = dict(data['users'][owner])
    user['name'] = 'clipboard-verify-local'
    user['password'] = {
        'salt': salt.hex(),
        'hash': hashlib.pbkdf2_hmac('sha256', password.encode(), salt, 600000).hex(),
        'iterations': 600000,
    }
    snapshot = {
        'version': data['version'],
        'users': {owner: user},
        'hosts': {str(HOST_ID): dict(host, pair_info=None)},
        'roles': {str(user['role_id']): data['roles'][str(user['role_id'])]},
    }
    # A foreign-owned host exists only in this isolated snapshot.
    foreign_id = '1234567'
    snapshot['hosts'][foreign_id] = dict(host, owner=987654321, pair_info=None)
    return snapshot, user, owner


def round_trip_samples():
    """Texts used for the write/read round trips when writes are authorized."""
    return [
        'Moonlight Windows 한글 😀\n두 번째 줄\t"quoted"',
        '',
        'a' * 65536,
    ]


def main(argv=None):
    parser = argparse.ArgumentParser(
        description='Verify the Windows clipboard bridge against an isolated copy of the '
                    'running web binary.',
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--authorize-destructive-test', action='store_true',
                        help='required before anything is written to the real clipboard')
    parser.add_argument('--guard-dir', type=Path, default=None,
                        help='staged copy of the guard state directory (ready/status/restored/done)')
    parser.add_argument('--run-nonce', default=None,
                        help='run nonce the guard must have published (default: random per run)')
    parser.add_argument('--session-id', type=int, default=None,
                        help='interactive session id the guard must report')
    parser.add_argument('--lease-seconds', type=float, default=180.0)
    parser.add_argument('--restore-timeout', type=float, default=30.0)
    args = parser.parse_args(argv)

    writes_allowed = bool(args.authorize_destructive_test)
    lease = None
    if writes_allowed:
        if args.guard_dir is None:
            print('REFUSING: --authorize-destructive-test requires --guard-dir', flush=True)
            return EXIT_LEASE_INVALID
        nonce = args.run_nonce or secrets.token_hex(16)
        lease = GuardLease(args.guard_dir, nonce, args.session_id, args.lease_seconds)
        try:
            state = lease.acquire()
        except LeaseError as exc:
            print('REFUSING: guard lease not usable: ' + str(exc), flush=True)
            return EXIT_LEASE_INVALID
        print('PASS: guard lease acquired for run ' + nonce + ' (session=' +
              str(parse_fields(state.get('ready')).get('session')) + ')', flush=True)
    else:
        print('INFO: read-only mode. No clipboard write will be attempted. Pass '
              '--authorize-destructive-test with a live --guard-dir to run write tests.',
              flush=True)

    # Staged privately by deployment; never echo token or config content.
    private_config = ROOT / 'windows-bridge-private.json'
    try:
        bridge_config = json.loads(private_config.read_text())
    except (OSError, ValueError) as exc:
        # Expected in the read-only CLI test, where no private config is staged.
        print('ABORT: staged bridge configuration ' + str(private_config) + ' is unavailable: '
              + exc.__class__.__name__ + '; nothing was written', flush=True)
        return EXIT_FAILED
    token = bridge_config['token']
    direct = 'http://' + bridge_config['bind'] + ':' + str(bridge_config.get('port', 47991))
    authorization = {'Authorization': 'Bearer ' + token}

    check('bridge rejects missing token', request(direct, '/clipboard')[0] == 401)
    check('bridge rejects wrong token', request(direct, '/clipboard', headers={'Authorization': 'Bearer ' + '0' * 64})[0] == 401)
    check('bridge rejects unknown path', request(direct, '/not-clipboard', headers=authorization)[0] == 404)
    check('bridge rejects unsupported method', request(direct, '/clipboard', method='DELETE', headers=authorization)[0] in (405, 501))
    assert_write_rejected(request, direct, authorization,
                          'bridge rejects malformed JSON', raw=b'{broken')
    assert_write_rejected(request, direct, authorization,
                          'bridge rejects NUL text', payload={'text': 'bad\0text'})
    assert_write_rejected(request, direct, authorization,
                          'bridge rejects oversized text', payload={'text': 'x' * 65537})
    # Raw TCP errors are tested with the same peer as production, not spoofed headers.
    def raw_status(extra, prefix='POST /clipboard HTTP/1.1\r\n'):
        with socket.create_connection((bridge_config['bind'], bridge_config['port']), timeout=8) as sock:
            sock.settimeout(8)
            wire = prefix + 'Host: workstation\r\nAuthorization: Bearer ' + token + '\r\n' + extra
            sock.sendall(wire.encode())
            received = b''
            while b'\r\n\r\n' not in received:
                part = sock.recv(8192)
                if not part:
                    break
                received += part
            return int(received.split(b' ', 2)[1])
    check('TCP rejects duplicate Content-Length', raw_status('Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}') == 400)
    check('TCP rejects Transfer-Encoding', raw_status('Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n') == 400)
    check('TCP bounds request headers', raw_status('X-Large: ' + 'x' * 9000 + '\r\n\r\n') == 431)
    check('TCP enforces absolute partial-body deadline', raw_status('Content-Length: 10\r\n\r\nx') == 408)

    # The bridge read path never mutates the clipboard, so it is safe either way.
    status, _, read_body = request(direct, '/clipboard', headers=authorization)
    check('bridge read path accepts the staged token and only reads', status in (200, 503))
    if status == 200:
        check('bridge read response has the documented shape',
              set(json.loads(read_body)) == {'has_text', 'text'})

    # Snapshot strips every real password and pairing credential before writing.
    prod = docker_json('config.json')
    data = docker_json('data.json')
    password = secrets.token_hex(24)
    salt = secrets.token_bytes(16)
    snapshot, user, owner = make_snapshot(data, password, salt)
    foreign_id = '1234567'
    prod['data_storage']['path'] = '/verify/data.json'
    prod['web_server'].update(bind_address='127.0.0.1:18081', default_user_id=None, forwarded_header=None, first_login_create_admin=False, session_cookie_secure=False)
    prod['clipboard_bridges'] = {str(HOST_ID): {'endpoint': bridge_config['bind'] + ':' + str(bridge_config.get('port', 47991)), 'token': token}}
    prod['log']['file_path'] = None
    verify_dir = ROOT / 'isolated-runtime'
    verify_dir.mkdir(mode=0o700, exist_ok=False)
    exit_code = EXIT_FAILED
    try:
        for name, value in [('config.json', prod), ('data.json', snapshot)]:
            p = verify_dir / name
            p.write_text(json.dumps(value))
            p.chmod(0o600)
        image = json.loads(subprocess.check_output(['docker', 'inspect', 'moonlight-web']))[0]['Image']
        uid = str(os.getuid())
        gid = str(os.getgid())
        # Preserve exact image, run as current user with a loopback-only independent store.
        subprocess.run(['docker', 'run', '-d', '--name', TEMP_CONTAINER, '--network', 'host', '--user', uid + ':' + gid,
                        '-e', 'BIND_ADDRESS=127.0.0.1:18081', '-v', str(verify_dir) + ':/verify',
                        '--entrypoint', '/moonlight-web/web-server', image, '--config-path', '/verify/config.json', 'run'], check=True, stdout=subprocess.DEVNULL)
        for attempt in range(30):
            try:
                if request(BASE, '/api/health')[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(0.3)
        else:
            raise RuntimeError('isolated exact-binary web server did not start')
        api_read = '/api/host/clipboard/read'
        check('web API rejects anonymous clipboard read', request(BASE, api_read, method='POST', payload={'host_id': HOST_ID}, headers={'Origin': BASE})[0] == 401)
        status, headers, _ = request(BASE, '/api/login', method='POST', payload={'name': user['name'], 'password': password})
        check('isolated real web binary authenticates test-only account', status == 200)
        cookie = headers['Set-Cookie'].split(';', 1)[0]
        auth = {'Cookie': cookie, 'Origin': BASE}
        status, _, body = request(BASE, '/api/host/clipboard?host_id=' + str(HOST_ID), headers=auth)
        check('Windows host advertises clipboard availability', status == 200 and json.loads(body)['available'] is True)
        evil = dict(auth, Origin='https://invalid.example')
        check('web API rejects wrong Origin', request(BASE, api_read, method='POST', payload={'host_id': HOST_ID}, headers=evil)[0] == 403)
        check('web API rejects unknown host', request(BASE, api_read, method='POST', payload={'host_id': 1}, headers=auth)[0] == 404)
        check('web API enforces host ownership', request(BASE, api_read, method='POST', payload={'host_id': int(foreign_id)}, headers=auth)[0] == 403)
        check('web response does not expose bridge credentials', token.encode() not in body)

        if writes_allowed and lease is not None:
            for i, text in enumerate(round_trip_samples()):
                lease.before_write()
                status, _, _ = request(BASE, '/api/host/clipboard', method='POST', payload={'host_id': HOST_ID, 'text': text}, headers=auth)
                check('web -> Windows clipboard sample ' + str(i), status == 204)
                status, _, body = request(direct, '/clipboard', headers=authorization)
                check('native Windows read matches web write sample ' + str(i), status == 200 and json.loads(body)['text'] == text)
                # A direct bridge write, followed by the authenticated web read, tests reverse routing.
                lease.before_write()
                status, _, _ = request(direct, '/clipboard', method='POST', payload={'text': text}, headers=authorization)
                check('direct Windows bridge write sample ' + str(i), status == 204)
                status, _, body = request(BASE, api_read, method='POST', payload={'host_id': HOST_ID}, headers=auth)
                check('Windows -> web clipboard sample ' + str(i), status == 200 and json.loads(body)['text'] == text)
        else:
            print('SKIP: clipboard round-trip samples (read-only mode: no write attempted)', flush=True)

        request(BASE, '/api/logout', method='POST', headers=auth)
        exit_code = EXIT_OK
    finally:
        subprocess.run(['docker', 'rm', '-f', TEMP_CONTAINER], check=True, stdout=subprocess.DEVNULL)
        shutil.rmtree(verify_dir)

    # Production auth remains closed; do not use real account credentials.
    check('production anonymous clipboard read stays denied', request('http://127.0.0.1:8080', '/api/host/clipboard/read', method='POST', payload={'host_id': HOST_ID}, headers={'Origin': 'http://127.0.0.1:8080'})[0] == 401)

    if lease is not None:
        try:
            state = lease.release(args.restore_timeout)
            lease.verify_restored(state)
        except LeaseError as exc:
            print('FAIL: clipboard restoration not proven: ' + str(exc), flush=True)
            return EXIT_RESTORE_UNVERIFIED
        print('PASS: guard restored and verified the clipboard after ' +
              str(lease.writes_performed) + ' write(s)', flush=True)
    else:
        print('INFO: no clipboard write was performed, so no restoration is required', flush=True)

    print('DONE: exact deployed Moonlight web binary and bridge (' +
          ('with' if writes_allowed else 'without') + ' real clipboard writes)', flush=True)
    return exit_code


if __name__ == '__main__':
    sys.exit(main())
