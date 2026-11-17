"""Verify Codex HTTP Responses, a free source, and Kimi fallback in an isolated profile."""

import argparse
import http.server
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import tempfile
import threading
import time
import urllib.request


class RateLimited(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get('content-length', '0')))
        self.send_response(429)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Retry-After', '180')
        self.end_headers()
        self.wfile.write(b'{"error":{"code":"account_quota_exhausted"}}')

    def log_message(self, *_):
        pass


def cli_turn(client, env, project):
    token = 'aubridge-' + secrets.token_hex(8)
    (project / 'probe.txt').write_text(token + '\n')
    args = [client, '--profile', 'aubridge', 'exec', '--sandbox', 'read-only',
            '--ephemeral', '--skip-git-repo-check', '--json',
            'Run cat probe.txt in the current directory using the shell tool, then reply with exactly the file content.']
    run = subprocess.run(args, env=env, cwd=project, stdin=subprocess.DEVNULL, text=True,
                         capture_output=True, timeout=180)
    if run.returncode:
        raise RuntimeError(f'Codex exit {run.returncode}\n{run.stdout[-2600:]}\n{run.stderr[-600:]}')
    events = [json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
    items = [event['item'] for event in events if event.get('type') == 'item.completed']
    assert any(item.get('type') == 'command_execution' and item.get('exit_code') == 0
               and token in item.get('aggregated_output', '') for item in items), items
    assert any(item.get('type') == 'agent_message' and token in item.get('text', '') for item in items), items
    assert any(event.get('type') == 'turn.completed' for event in events)


def check(gateway, client, root, replay, free_model):
    home, project = root / 'codex', root / 'project'
    home.mkdir()
    project.mkdir()
    original = '# Unrelated default config\nmodel = "gpt-5.1"\napproval_policy = "never"\n'
    config = home / 'config.toml'
    profile = home / 'aubridge.config.toml'
    config.write_text(original)
    env = {'HOME': str(root), 'CODEX_HOME': str(home), 'PATH': os.environ['PATH']}
    gateway_env = {**os.environ, **env, 'PLUGINS_DIR': str(root / 'gateway/plugins'),
                   'CLAUDE_CONFIG_DIR': str(root / 'claude'), 'KEY_CODEX_REPLAY_429': 'fixture'}
    for name in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'):
        gateway_env.pop(name, None)
    process = subprocess.Popen([gateway], env=gateway_env, stdout=subprocess.DEVNULL)
    try:
        session_file = root / 'gateway/gateway-session.json'
        for _ in range(100):
            if session_file.exists() or process.poll() is not None:
                break
            time.sleep(0.05)
        session = json.loads(session_file.read_text())
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def admin(path, body=None):
            payload = json.dumps(body).encode() if body is not None else None
            request = urllib.request.Request(session['endpoint'] + '/admin/' + path, payload,
                                             {'Authorization': 'Bearer ' + session['admin_token'],
                                              'Content-Type': 'application/json'})
            with opener.open(request, timeout=20) as response:
                return json.load(response)

        admin('providers', {'id': 'codex-replay-429', 'base': f'http://127.0.0.1:{replay.server_port}/v1',
                            'models': ['codex-replay-free']})
        directory = admin('providers/openrouter/catalog', {})
        assert directory['issue'] is None
        assert any(model['id'] == free_model and model['tools'] for model in directory['models'])
        cases = [('validation-codex-free', free_model, 'openrouter'),
                 ('validation-codex-kimi', 'codex-replay-free', 'kimi-code')]
        for group, free, _ in cases:
            admin('model-groups', {'name': group, 'members': ['kimi-k3', free], 'rule': 'free_first'})
        admin('agents/codex/enable', {'on': True})
        assert config.read_text() == original
        assert 'wire_api = "responses"' in profile.read_text()
        for group, _, provider in cases:
            admin('agents/codex/model', {'model': group})
            try:
                cli_turn(client, env, project)
            finally:
                rows = [row for row in admin('gateway')['requests']
                        if row['model'] == group and row['agent'] == 'codex']
                print(group, [(row['provider'], row['served_model'], row['outcome'], row['input_tokens'],
                               row['output_tokens'], [(a['id'], a.get('reason')) for a in row['attempts']])
                              for row in rows], flush=True)
            passed = [row for row in rows if row['provider'] == provider and row['outcome'] == 'ok'
                      and row['input_tokens'] is not None and row['output_tokens'] is not None]
            assert len(passed) >= 2, rows
            if provider == 'kimi-code':
                assert any(a.get('reason') == 'account-quota-exhausted' for row in passed for a in row['attempts'])

        preview = admin('agents/codex/disconnect-preview')
        assert preview == {'blocked': False, 'changes': [{'field': profile.name, 'action': 'remove'}]}
        admin('agents/codex/enable', {'on': False, 'preview': preview})
        assert not profile.exists() and config.read_text() == original
        print('Codex profile removed and default config preserved', flush=True)
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('gateway', help='path to the aubridged binary')
parser.add_argument('client', help='path to the Codex CLI binary')
parser.add_argument('--free-model', default='nvidia/nemotron-3.5-super-vl:free')
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='aubridge-codex-') as temp, \
        http.server.ThreadingHTTPServer(('127.0.0.1', 0), RateLimited) as replay:
    threading.Thread(target=replay.serve_forever, daemon=True).start()
    try:
        check(args.gateway, args.client, Path(temp).resolve(), replay, args.free_model)
    finally:
        replay.shutdown()
