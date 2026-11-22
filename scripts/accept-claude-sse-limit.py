import http.server
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request


class Replay(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        event = {'stage':'headers', 'path_kind':'count_tokens' if self.path.endswith('/count_tokens')
                 else 'messages' if self.path.endswith('/messages') else 'other',
                 'content_length_present':self.headers.get('content-length') is not None,
                 'transfer_encoding':'chunked' if self.headers.get('transfer-encoding') == 'chunked'
                 else 'other' if self.headers.get('transfer-encoding') else 'absent'}
        self.server.transport_events.append(event)
        try:
            length = int(self.headers.get('content-length', 0))
            event['content_length'] = length
            self.connection.settimeout(5)
            raw = self.rfile.read(length)
            event['received_bytes'] = len(raw)
            payload = json.loads(raw)
            event['stage'] = 'json_parsed'
            self.reply(payload)
            event['stage'] = 'response_written'
        except Exception as error:
            event['error_class'] = type(error).__name__
            raise

    def reply(self, payload):
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



def safe_metadata(rows):
    allowed = {'limited', 'backup', 'anthropic-limited', 'anthropic-backup',
               'validation-anthropic', 'claude-code', 'ok', 'failed', 'cancelled'}

    def entry(row):
        result = {}
        for key in ('id', 'model', 'agent', 'provider', 'outcome'):
            if row.get(key) in allowed:
                result[key] = row[key]
        request_id = row.get('request_id')
        if isinstance(request_id, str) and re.fullmatch(r'[A-Za-z0-9_-]{1,128}', request_id):
            result['request_id'] = request_id
        reason = row.get('reason')
        if isinstance(reason, str) and re.fullmatch(r'[a-z][a-z0-9-]{0,79}', reason):
            result['reason'] = reason
        for key in ('status', 'ts', 'duration_ms', 'input_tokens', 'output_tokens'):
            if isinstance(row.get(key), int):
                result[key] = row[key]
        return result

    result = []
    for row in rows:
        if row.get('model') != 'validation-anthropic':
            continue
        clean = entry(row)
        for key in ('attempts', 'skipped'):
            clean[key] = [entry(item) for item in row.get(key, []) if isinstance(item, dict)]
        result.append(clean)
    return result



def safe_readiness(value):
    return {'serving':value.get('serving'),
            'candidates':[{key:item[key] for key in ('id', 'model', 'kind') if key in item}
                          for item in value.get('candidates', []) if item.get('id') in ('limited', 'backup')],
            'skipped':safe_metadata([{'model':'validation-anthropic',
                                     'skipped':value.get('skipped', [])}])[0]['skipped']}


def write_diagnostics(receipt):
    output = Path(__file__).resolve().parents[1] / '.local/iterations/0113'
    output.mkdir(parents=True, exist_ok=True)
    path = output / f'controlled-claude-sse-limit-diagnostics-{time.time_ns()}-{os.getpid()}.json'
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as stream:
        json.dump(receipt, stream, indent=2)
        stream.write('\n')
    print(f'Controlled replay diagnostics: {path}')


def check(binary, cli, home, replay):
    project = home / 'project'
    project.mkdir()
    env = {'PATH':os.environ['PATH'], 'HOME':str(home), 'CLAUDE_CONFIG_DIR':str(home / 'claude'),
           'DISABLE_NONESSENTIAL_TRAFFIC':'1', 'DISABLE_AUTOUPDATER':'1',
           # reqwest system-proxy otherwise routes this loopback replay through macOS proxies.
           'NO_PROXY':'127.0.0.1,localhost,::1', 'no_proxy':'127.0.0.1,localhost,::1'}
    gateway_env = {**env, 'PLUGINS_DIR':str(home / 'gateway/plugins'), 'CODEX_HOME':str(home / 'codex')}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    receipt = {'scope':'controlled-loopback-replay', 'stage':'launch', 'turns':[],
               'private_loopback_proxy_bypass':True}
    admin = None
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

            receipt['stage'] = 'fixture_setup'
            for name, kind in [('limited','free_tier'), ('backup','subscription')]:
                admin('providers', {'id':name, 'base':f'http://127.0.0.1:{replay.server_port}/{name}/v1',
                                    'protocol':'anthropic','kind':kind,'models':['anthropic-' + name]})
            group = 'validation-anthropic'
            admin('model-groups', {'name':group, 'members':['anthropic-backup','anthropic-limited'], 'rule':'free_first'})
            admin('agents/claude-code/enable', {'on':True})
            receipt['initial_readiness'] = safe_readiness(admin('routing/preview?model=validation-anthropic'))
            receipt['stage'] = 'cli_turns'
            for index in range(2):
                prior = len(replay.calls)
                turn = {'index':index, 'timeout':False, 'exit_code':None, 'error_categories':[]}
                receipt['turns'].append(turn)
                try:
                    result = subprocess.run([cli, '-p', '--model', group, '--tools', '', '--strict-mcp-config',
                        '--no-session-persistence', '--output-format', 'json', '--', 'Reply briefly.'],
                        cwd=project, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=40)
                except subprocess.TimeoutExpired:
                    turn['timeout'] = True
                    turn['error_categories'] = ['cli_timeout']
                    raise RuntimeError('cli_timeout') from None
                turn['exit_code'] = result.returncode
                combined = result.stdout + result.stderr
                turn['error_categories'] = [category for category in (
                    'authentication_error', 'rate_limit_error', 'api_error', 'permission_denied',
                    'ECONNREFUSED', 'ETIMEDOUT') if category in combined]
                assert result.returncode == 0, 'cli_nonzero_exit'
                try:
                    reply = json.loads(result.stdout)
                except json.JSONDecodeError:
                    turn['error_categories'].append('invalid_cli_json')
                    raise RuntimeError('invalid_cli_json') from None
                turn['is_error'] = reply.get('is_error') is True
                turn['expected_reply_matched'] = reply.get('result') == 'anthropic-replay-ok'
                assert turn['expected_reply_matched'], 'cli_reply_mismatch'
                calls = replay.calls[prior:]
                if index == 0:
                    assert calls.pop(0) == 'anthropic-limited'
                assert calls and set(calls) == {'anthropic-backup'}, calls
            receipt['stage'] = 'backoff_assertions'
            rows = [row for row in admin('gateway')['requests'] if row['model'] == group]
            assert len(rows) == len(replay.calls) - 1 and all(row['agent'] == 'claude-code' and row['outcome'] == 'ok'
                                          and row['provider'] == 'backup' for row in rows), 'decision_metadata_mismatch'
            assert rows[-1]['attempts'][0]['reason'] == 'account-rate-limit'
            assert all(row['skipped'][0]['reason'] == 'account-rate-limit' for row in rows[:-1])
            receipt['stage'] = 'passed'
            print(f'Claude Code Anthropic replay: 2 CLI turns, {len(rows)} successful requests, 1 hit on the throttled source')
        except Exception as error:
            receipt['failure_class'] = type(error).__name__
            raise
        finally:
            receipt['replay_calls'] = [model if model in ('anthropic-limited', 'anthropic-backup')
                                       else 'unexpected_model' for model in replay.calls]
            receipt['transport_events'] = replay.transport_events
            receipt['gateway_exit_code'] = gateway.poll()
            if admin is not None:
                for phase in ('preview', 'history', 'settings'):
                    try:
                        if phase == 'preview':
                            value = admin('routing/preview?model=validation-anthropic')
                            receipt['readiness'] = safe_readiness(value)
                        elif phase == 'history':
                            value = admin('gateway')
                            receipt['history_status'] = value.get('history_status')
                            receipt['decisions'] = safe_metadata(value.get('requests') or [])
                        else:
                            value = admin('settings')
                            receipt['cost_intent'] = value.get('cost_intent')
                            receipt['cost_policy_issue'] = value.get('cost_policy_issue')
                    except Exception as error:
                        receipt[phase + '_failure_class'] = type(error).__name__
            try:
                wiring = json.loads((home / 'claude/settings.json').read_text()).get('env', {})
                receipt['managed_wiring'] = {
                    'base_present':bool(wiring.get('ANTHROPIC_BASE_URL')),
                    'auth_present':bool(wiring.get('ANTHROPIC_AUTH_TOKEN')),
                    'target_matches_private_gateway':wiring.get('ANTHROPIC_BASE_URL') == session['endpoint']}
            except Exception as error:
                receipt['wiring_failure_class'] = type(error).__name__
            try:
                write_diagnostics(receipt)
            finally:
                gateway.terminate()
                gateway.wait(timeout=10)


with tempfile.TemporaryDirectory(prefix='aubridge-claude-') as directory, \
        http.server.ThreadingHTTPServer(('127.0.0.1', 0), Replay) as replay:
    replay.calls = []
    replay.transport_events = []
    threading.Thread(target=replay.serve_forever, daemon=True).start()
    try:
        check(sys.argv[1], sys.argv[2], Path(directory), replay)
    finally:
        replay.shutdown()
