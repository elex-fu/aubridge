import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request


class Replay(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get('content-length', 0))))
        if self.path.endswith('/count_tokens'):
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(b'{"input_tokens":20}')
            return
        assert self.path.endswith('/messages')
        model = payload['model']
        self.server.calls.append(model)
        start = {'type':'message_start', 'message':{'id':'msg_replay', 'type':'message', 'role':'assistant',
                 'model':model, 'content':[], 'stop_reason':None, 'stop_sequence':None,
                 'usage':{'input_tokens':20, 'output_tokens':0}}}
        limited = model == 'anthropic-limited'
        frames = [start, {'type':'error','error':{'type':'rate_limit_error','scope':'account','message':'fixture limit'}}] if limited else [
            start,
            {'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},
            {'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'anthropic-replay-ok'}},
            {'type':'content_block_stop','index':0},
            {'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':4}},
            {'type':'message_stop'},
        ]
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        if limited:
            self.send_header('Retry-After', '120')
        self.end_headers()
        for frame in frames:
            self.wfile.write(('event: ' + frame['type'] + '\ndata: ' + json.dumps(frame) + '\n\n').encode())

    def log_message(self, *_):
        pass


def check(binary, cli, home, replay):
    project = home / 'project'
    project.mkdir()
    env = {'PATH':os.environ['PATH'], 'HOME':str(home), 'CLAUDE_CONFIG_DIR':str(home / 'claude'),
           'DISABLE_NONESSENTIAL_TRAFFIC':'1', 'DISABLE_AUTOUPDATER':'1'}
    gateway_env = {**env, 'PLUGINS_DIR':str(home / 'gateway/plugins'), 'CODEX_HOME':str(home / 'codex')}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with subprocess.Popen([binary], cwd=project, env=gateway_env, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL) as gateway:
        try:
            session_file = home / 'gateway/gateway-session.json'
            for _ in range(100):
                if session_file.exists() or gateway.poll() is not None:
                    break
                time.sleep(0.05)
            session = json.loads(session_file.read_text())

            def admin(path, body=None):
                payload = json.dumps(body).encode() if body is not None else None
                req = urllib.request.Request(session['endpoint'] + '/admin/' + path, payload,
                                              {'Authorization':'Bearer ' + session['admin_token'],
                                               'Content-Type':'application/json'})
                with opener.open(req, timeout=10) as response:
                    return json.load(response)

            for name, kind in [('limited','free_tier'), ('backup','subscription')]:
                admin('providers', {'id':name, 'base':f'http://127.0.0.1:{replay.server_port}/{name}/v1',
                                    'protocol':'anthropic','kind':kind,'models':['anthropic-' + name]})
            group = 'validation-anthropic'
            admin('model-groups', {'name':group, 'members':['anthropic-backup','anthropic-limited'], 'rule':'free_first'})
            admin('agents/claude-code/enable', {'on':True})
            for index in range(2):
                prior = len(replay.calls)
                result = subprocess.run([cli, '-p', '--model', group, '--tools', '', '--strict-mcp-config',
                    '--no-session-persistence', '--output-format', 'json', '--', 'Reply briefly.'],
                    cwd=project, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=40)
                assert result.returncode == 0, (result.stdout[-800:], result.stderr[-800:])
                assert json.loads(result.stdout)['result'] == 'anthropic-replay-ok'
                calls = replay.calls[prior:]
                if index == 0:
                    assert calls.pop(0) == 'anthropic-limited'
                assert calls and set(calls) == {'anthropic-backup'}, calls
            rows = [row for row in admin('gateway')['requests'] if row['model'] == group]
            assert len(rows) == len(replay.calls) - 1 and all(row['agent'] == 'claude-code' and row['outcome'] == 'ok'
                                          and row['provider'] == 'backup' for row in rows), rows
            assert rows[-1]['attempts'][0]['reason'] == 'account-rate-limit'
            assert all(row['skipped'][0]['reason'] == 'account-rate-limit' for row in rows[:-1])
            print(f'Claude Code Anthropic replay: 2 CLI turns, {len(rows)} successful requests, 1 hit on the throttled source')
        finally:
            gateway.terminate()
            gateway.wait(timeout=10)


with tempfile.TemporaryDirectory(prefix='aubridge-claude-') as directory, \
        http.server.ThreadingHTTPServer(('127.0.0.1', 0), Replay) as replay:
    replay.calls = []
    threading.Thread(target=replay.serve_forever, daemon=True).start()
    try:
        check(sys.argv[1], sys.argv[2], Path(directory), replay)
    finally:
        replay.shutdown()
