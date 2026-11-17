import argparse
import concurrent.futures
import http.server
import json
import os
import pathlib
import secrets
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from email.utils import formatdate


class Replay(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('content-length', '0')))
        if self.path.startswith('/context-small/'):
            self.server.context_calls.append('small')
            self.send_response(400)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"context_length_exceeded"}}')
        elif self.path.startswith('/context-wide/'):
            self.server.context_calls.append('wide')
            self.send_text(json.loads(body)['model'], 'context-fixture-ok')
        elif self.path.startswith('/cooldown/'):
            self.server.cooldown_calls.append(time.monotonic())
            if len(self.server.cooldown_calls) == 1:
                retry_after = formatdate(time.time() + 8, usegmt=True) if self.server.dated_cooldown else 3
                self.send_limit('model_rate_limit_exceeded', retry_after)
            else:
                self.send_text(json.loads(body)['model'], 'cooldown-fixture-ok')
        elif self.path.startswith('/embedded-first/'):
            self.server.embedded_calls.append('first')
            time.sleep(0.04)
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Retry-After', '60')
            self.end_headers()
            self.wfile.write(b'data: {"error":{"code":429,"scope":"model"}}\n\n')
        elif self.path.startswith('/embedded-second/'):
            self.server.embedded_calls.append('second')
            time.sleep(0.04)
            self.send_text(json.loads(body)['model'], 'embedded-fixture-ok')
        elif self.path.startswith('/pause-free/'):
            if self.server.pause_fail_free:
                self.send_error(503, 'fixture unavailable')
            else:
                self.send_text(json.loads(body)['model'], 'pause-free-ok')
        elif self.path.startswith('/pause-plan/'):
            self.server.pause_plan_calls += 1
            self.send_text(json.loads(body)['model'], 'pause-plan-ok')
        elif self.path.startswith('/resume-free/'):
            self.server.resume_free_calls += 1
            if self.server.resume_free_calls == 1:
                self.send_error(403, 'account has been suspended')
            else:
                self.send_text(json.loads(body)['model'], 'resume-free-ok')
        elif self.path.startswith('/edit-models/'):
            model = json.loads(body)['model']
            self.server.edited_model_calls.append(model)
            self.send_text(model, 'model-edit-ok')
        elif self.path.startswith('/queued-edit/'):
            self.server.queued_model_calls += 1
            self.send_text(json.loads(body)['model'], 'held-slot', self.server.queue_release)
        elif self.path.startswith('/probe-slots/'):
            request = json.loads(body)
            if any(message.get('content') == 'Reply with OK.' for message in request['messages']):
                self.server.probe_calls += 1
                self.send_text(request['model'], 'probe-ok')
            else:
                self.server.probe_stream_calls += 1
                self.send_text(request['model'], 'held-slot', self.server.queue_release)
        elif self.path.startswith('/queued-replaced/'):
            self.server.rebound_calls += 1
            self.send_text(json.loads(body)['model'], 'unexpected-rebound')
        elif self.path.startswith('/inflight-old/'):
            self.server.inflight_started.set()
            self.server.inflight_release.wait(timeout=12)
            self.send_error(403, 'account has been suspended')
        elif self.path.startswith('/inflight-new/'):
            self.send_text(json.loads(body)['model'], 'new-source-ok')
        else:
            self.send_limit('account_quota_exhausted', 180)

    def send_limit(self, code, retry_after):
        self.send_response(429)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Retry-After', str(retry_after))
        self.end_headers()
        self.wfile.write(json.dumps({'error': {'code': code}}).encode())

    def send_text(self, model, text, hold=None):
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        envelope = {'id': 'chatcmpl-replay', 'object': 'chat.completion.chunk', 'created': 1, 'model': model}
        chunks = [
            {'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': text}, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]},
            {'choices': [], 'usage': {'prompt_tokens': 1800, 'completion_tokens': 4, 'total_tokens': 1804}},
        ]
        for index, chunk in enumerate(chunks):
            self.wfile.write(('data: ' + json.dumps({**envelope, **chunk}) + '\n\n').encode())
            if index == 0 and hold is not None:
                hold.wait(timeout=12)
        self.wfile.write(b'data: [DONE]\n\n')

    def log_message(self, *_):
        pass


INITIAL = '''{
  "$schema": "https://opencode.ai/config.json",
  // fixture comment outside the managed provider
  "autoupdate": false,
  "share": "disabled",
  "permission": {"*": "deny", "read": "allow"},
  "enabled_providers": ["aubridge"],
  "provider": {
    // fixture comment before the managed provider
    "aubridge": {
      // fixture comment inside the original provider
      "name": "Original",
      "npm": "@ai-sdk/openai-compatible",
      "options": {"baseURL": "http://127.0.0.1:1/v1", "apiKey": "fixture"},
      "models": {"original": {"name": "Original"}},
    },
  },
}
'''


def run_cli(binary, args, env, directory):
    result = subprocess.run([binary, *args], env=env, cwd=directory,
                            capture_output=True, text=True, timeout=180)
    if result.returncode:
        print('CLI exit', result.returncode, result.stderr[-1800:], result.stdout[-900:])
        raise RuntimeError('OpenCode command failed')
    return result.stdout


def seed_context_replay(directory, replay, reject_first=False):
    directory.mkdir(parents=True)
    sources = [('small', 'free_tier', 128), ('wide', 'subscription', 32768)]
    for name, kind, window in sources:
        model = 'context-' + name
        manifest = {'id': model, 'version': '1', 'kind': kind, 'free_models': [model],
                    'permissions': {'net': ['127.0.0.1']},
                    'provider': {'base': f'http://127.0.0.1:{replay.server_port}/{model}', 'auth': 'bearer'}}
        if name == 'wide' or not reject_first:
            manifest['context_windows'] = {model: window}
        (directory / f'{model}.json').write_text(json.dumps(manifest))
    (directory / 'enabled.json').write_text(json.dumps({'context-small': True, 'context-wide': True}))


def check_context_replay(client, env, project, replay, admin, reject_first=False):
    group = 'validation-context'
    admin('model-groups', {'name': group, 'members': ['context-small', 'context-wide'], 'rule': 'free_first'})
    admin('agents/opencode/enable', {'on': True})
    prompt = 'Read this sample input and reply briefly. ' + 'routing context sample words. ' * 150
    output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                              '--dir', str(project), '--', prompt], env, project)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(event.get('type') == 'text' and event['part']['text'] == 'context-fixture-ok'
               for event in events), output[-1500:]
    rows = [row for row in admin('gateway')['requests'] if row['model'] == group and row['outcome'] == 'ok']
    assert rows and all(row['provider'] == 'context-wide' for row in rows)
    if reject_first:
        assert replay.context_calls.count('small') == replay.context_calls.count('wide') == len(rows)
        assert all(row['attempts'][0].get('reason') == 'context_length_exceeded' for row in rows)
        print(f'OpenCode context failover replay: {len(rows)} requests recovered on the declared wider model')
        return
    assert replay.context_calls and all(source == 'wide' for source in replay.context_calls)
    skips = [entry for row in rows for entry in row['skipped'] if entry['id'] == 'context-small']
    assert skips and all(entry['reason'] == 'context-window-too-small' and
                         entry['estimated_input_tokens'] > entry['context_window'] == 128 for entry in skips)
    print('OpenCode context replay:', len(rows), 'successful requests, zero small-model calls,',
          'estimated tokens', [entry['estimated_input_tokens'] for entry in skips], flush=True)


def check_cooldown_replay(client, env, project, replay, admin):
    model = 'validation-cooldown'
    admin('providers', {'id': 'replay-cooldown', 'base': f'http://127.0.0.1:{replay.server_port}/cooldown/v1',
                        'models': [model]})
    admin('agents/opencode/enable', {'on': True})
    output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{model}',
                              '--dir', str(project), '--', 'Reply briefly to this retry test.'], env, project)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(event.get('type') == 'text' and event['part']['text'] == 'cooldown-fixture-ok'
               for event in events), output[-1500:]
    rows = [row for row in admin('gateway')['requests'] if row['model'] == model and row['agent'] == 'opencode']
    assert any(row['outcome'] == 'rate-limited' and
               any(attempt['status'] == 429 for attempt in row['attempts']) for row in rows)
    assert any(row['outcome'] == 'ok' and row['provider'] == 'replay-cooldown' for row in rows)
    elapsed = replay.cooldown_calls[1] - replay.cooldown_calls[0]
    assert elapsed >= (7 if replay.dated_cooldown else 2), f'an upstream retry arrived early ({elapsed:.2f}s)'
    header = 'date' if replay.dated_cooldown else 'seconds'
    print(f'OpenCode cooldown replay ({header}): 429 then success, next upstream request after {elapsed:.2f}s', flush=True)


def check_embedded_replay(client, env, project, replay, admin):
    models = ['embedded-first', 'embedded-second']
    for model in models:
        admin('providers', {'id': model, 'base': f'http://127.0.0.1:{replay.server_port}/{model}/v1',
                            'models': [model]})
    group = 'validation-embedded'
    admin('model-groups', {'name': group, 'members': models, 'rule': 'free_first'})
    admin('agents/opencode/enable', {'on': True})
    for _ in range(2):
        output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                                  '--dir', str(project), '--', 'Reply briefly to this failover test.'], env, project)
        events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
        assert any(event.get('type') == 'text' and event['part']['text'] == 'embedded-fixture-ok'
                   for event in events), output[-1500:]
    rows = [row for row in admin('gateway')['requests'] if row['model'] == group and row['agent'] == 'opencode']
    assert len(rows) >= 2 and all(row['outcome'] == 'ok' and row['provider'] == 'embedded-second' for row in rows)
    switched = next(row for row in rows if any(attempt['status'] == 200 and attempt.get('reason') == 'model-rate-limit'
                                               for attempt in row['attempts']))
    assert all(0 <= row['first_content_ms'] <= row['duration_ms'] for row in rows)
    assert switched['first_content_ms'] >= 70
    assert all(35 <= attempt['duration_ms'] <= switched['duration_ms'] for attempt in switched['attempts'])
    assert any(entry['id'] == 'embedded-first' and entry['reason'] == 'model-rate-limit'
               for row in rows for entry in row['skipped'])
    assert replay.embedded_calls.count('first') == 1
    check = admin('providers/embedded-first/check', {})
    assert check['status'] == 'model-rate-limit' and check['upstream_status'] == 200
    print(f'OpenCode embedded replay: {len(rows)} successful requests, one inference attempt on the limited model; connection check reported model-rate-limit', flush=True)
    print('Failover timing (ms): first content', switched['first_content_ms'], 'total', switched['duration_ms'],
          'attempts', [attempt['duration_ms'] for attempt in switched['attempts']], flush=True)


def check_pause_replay(client, env, project, replay, admin):
    for model, kind in [('pause-free', 'free_tier'), ('pause-plan', 'subscription')]:
        admin('providers', {'id': model, 'base': f'http://127.0.0.1:{replay.server_port}/{model}/v1',
                            'models': [model], 'kind': kind})
    group = 'validation-pause'
    admin('model-groups', {'name': group, 'members': ['pause-plan', 'pause-free']})
    admin('agents/opencode/enable', {'on': True})
    for paused, expected in [(True, 'pause-free'), (False, 'pause-plan')]:
        admin('providers/pause-plan/pause', {'paused': paused})
        replay.pause_fail_free = not paused
        output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                                  '--dir', str(project), '--', 'Reply briefly to this routing test.'], env, project)
        events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
        assert any(event.get('type') == 'text' and event['part']['text'] == expected + '-ok'
                   for event in events), output[-1500:]
        rows = [row for row in admin('gateway')['requests'] if row['model'] == group and row['provider'] == expected]
        assert rows and all(row['outcome'] == 'ok' for row in rows)
        if paused:
            assert replay.pause_plan_calls == 0
            assert all(any(item['id'] == 'pause-plan' and item['reason'] == 'provider-paused'
                           for item in row['skipped']) for row in rows)
        else:
            assert replay.pause_plan_calls == len(rows)
            assert all(row['attempts'][0]['id'] == 'pause-free' and row['attempts'][0]['status'] == 503 for row in rows)
        print(f'OpenCode pause={paused}: {len(rows)} successful requests via {expected}; subscription calls {replay.pause_plan_calls}', flush=True)


def check_resume_replay(client, env, project, replay, admin):
    for model, kind in [('resume-free', 'free_tier'), ('pause-plan', 'subscription')]:
        admin('providers', {'id': model, 'base': f'http://127.0.0.1:{replay.server_port}/{model}/v1',
                            'models': [model], 'kind': kind})
    group = 'validation-resume'
    admin('model-groups', {'name': group, 'members': ['pause-plan', 'resume-free']})
    admin('agents/opencode/enable', {'on': True})

    def turn(expected):
        output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                                  '--dir', str(project), '--', 'Reply briefly to this recovery test.'], env, project)
        events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
        assert any(event.get('type') == 'text' and event['part']['text'] == expected for event in events)

    def blocked():
        providers = admin('providers')['providers']
        return next(p['routing_disabled'] for p in providers if p['id'] == 'resume-free')

    turn('pause-plan-ok')
    assert replay.resume_free_calls == 1 and replay.pause_plan_calls > 0 and blocked()
    assert admin('providers/resume-free/check', {})['status'] == 'ok'
    assert replay.resume_free_calls == 2 and blocked()
    plan_calls = replay.pause_plan_calls
    assert admin('providers/resume-free/resume-routing', {})['routing_disabled'] is False
    turn('resume-free-ok')
    rows = [r for r in admin('gateway')['requests'] if r['model'] == group and r['outcome'] == 'ok']
    recovered = sum(r['provider'] == 'resume-free' for r in rows)
    assert recovered == replay.resume_free_calls - 2 and recovered > 0 and replay.pause_plan_calls == plan_calls
    assert any(a['status'] == 403 for row in rows for a in row['attempts'])
    print(f'OpenCode routing resume: 403 then {plan_calls} subscription requests, {recovered} free requests after resume', flush=True)


def check_free_only_replay(client, env, project, replay, admin):
    sources = [('restricted-plan', 'pause-plan', 'subscription'),
               ('unavailable-free', 'pause-free', 'free_tier'),
               ('working-free', 'context-wide', 'free_tier')]
    for model, path, kind in sources:
        admin('providers', {'id': model, 'base': f'http://127.0.0.1:{replay.server_port}/{path}/v1',
                            'models': [model], 'kind': kind})
    group = 'validation-free-only'
    admin('model-groups', {'name': group, 'members': [entry[0] for entry in sources], 'rule': 'free_only'})
    admin('agents/opencode/enable', {'on': True})
    discovered = run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
    assert f'aubridge/{group}' in discovered
    replay.pause_fail_free = True
    output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                              '--dir', str(project), '--', 'Reply briefly to this free-only test.'], env, project)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(event.get('type') == 'text' and event['part']['text'] == 'context-fixture-ok'
               for event in events), output[-1500:]
    rows = [row for row in admin('gateway')['requests'] if row['model'] == group and row['outcome'] == 'ok']
    assert rows and all(row['provider'] == 'working-free' for row in rows)
    assert any(row['attempts'][0]['id'] == 'unavailable-free' and row['attempts'][0]['status'] == 503
               for row in rows)
    assert all(any(item['id'] == 'restricted-plan' and item['reason'] == 'subscription-excluded'
                   for item in row['skipped']) for row in rows)
    assert replay.pause_plan_calls == 0
    print(f'OpenCode free-only replay: {len(rows)} successful free requests after 503; zero subscription calls', flush=True)


def check_model_edit_replay(client, env, project, replay, admin):
    admin('providers', {'id': 'edit-source', 'base': f'http://127.0.0.1:{replay.server_port}/edit-models/v1',
                        'models': ['edit-old']})
    admin('agents/opencode/enable', {'on': True})
    before = run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
    assert 'aubridge/edit-old' in before
    changed = admin('providers/edit-source/models', {'models': ['edit-new']})
    assert changed['models'] == ['edit-new'] and changed['opencode_issue'] is None
    after = run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
    assert 'aubridge/edit-new' in after and 'aubridge/edit-old' not in after
    output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', 'aubridge/edit-new',
                              '--dir', str(project), '--', 'Reply briefly to this model edit test.'], env, project)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(event.get('type') == 'text' and event['part']['text'] == 'model-edit-ok' for event in events)
    assert replay.edited_model_calls and set(replay.edited_model_calls) == {'edit-new'}
    print(f'OpenCode model edit replay: replaced discovery and sent {len(replay.edited_model_calls)} requests to the new model', flush=True)


def open_chat_stream(endpoint, model):
    payload = json.dumps({'model': model, 'stream': True,
                          'messages': [{'role': 'user', 'content': 'Hold one fixture slot'}]}).encode()
    request = urllib.request.Request(endpoint + '/v1/chat/completions', payload,
                                     {'Authorization': 'Bearer aubridge-probe', 'Content-Type': 'application/json'})
    return urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10)


def check_queued_edit_replay(client, env, project, replay, admin, rebind=False):
    for name, model, path, kind in [('queued-source', 'queue-old', 'queued-edit', 'subscription'),
                                    ('queue-backup', 'queue-backup', 'context-wide', 'free_tier')]:
        admin('providers', {'id': name, 'base': f'http://127.0.0.1:{replay.server_port}/{path}/v1',
                            'models': [model], 'kind': kind})
    group = 'validation-queue'
    admin('model-groups', {'name': group, 'members': ['queue-old', 'queue-backup'], 'rule': 'sequential'})
    admin('agents/opencode/enable', {'on': True})
    assert f'aubridge/{group}' in run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
    endpoint = admin('gateway')['endpoint']
    with open_chat_stream(endpoint, 'queue-old'), open_chat_stream(endpoint, 'queue-old'), \
            concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        assert replay.queued_model_calls == 2
        job = pool.submit(run_cli, client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                                           '--dir', str(project), '--', 'Reply briefly to this queued request.'], env, project)
        try:
            time.sleep(3.0)
            if rebind:
                admin('providers/queued-source', method='DELETE')
                admin('providers', {'id': 'queued-source', 'models': ['queue-old'], 'kind': 'subscription',
                                    'base': f'http://127.0.0.1:{replay.server_port}/queued-replaced/v1'})
            else:
                admin('providers/queued-source/models', {'models': ['queue-new']})
        finally:
            replay.queue_release.set()
        output = job.result(timeout=30)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(event.get('type') == 'text' and event['part']['text'] == 'context-fixture-ok' for event in events)
    rows = [row for row in admin('gateway')['requests'] if row['model'] == group and row['outcome'] == 'ok']
    assert rows and all(row['provider'] == 'queue-backup' for row in rows)
    reason = 'provider-changed' if rebind else 'model-unlisted'
    assert any(a['id'] == 'queued-source' and a.get('reason') == reason and a['status'] == 0
               for row in rows for a in row['attempts'][:-1]), [(row['attempts'], row['skipped']) for row in rows]
    assert replay.queued_model_calls == 2
    assert replay.rebound_calls == 0
    print(f'OpenCode queued replay ({reason}): {len(rows)} successful fallback requests; only 2 original slot holders reached the old model', flush=True)


def check_inflight_rebind_replay(client, env, project, replay, admin):
    source = {'id':'inflight-source', 'models':['inflight-model'], 'kind':'free_tier'}
    base = f'http://127.0.0.1:{replay.server_port}'
    admin('providers', {**source, 'base':base + '/inflight-old/v1'})
    admin('providers', {'id':'inflight-backup','models':['backup-model'], 'kind':'subscription',
                        'base':base + '/pause-plan/v1'})
    group = 'validation-inflight'
    admin('model-groups', {'name':group,'members':['backup-model','inflight-model']})
    admin('agents/opencode/enable', {'on':True})

    def turn():
        output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', f'aubridge/{group}',
                                  '--dir', str(project), '--', 'Reply briefly.'], env, project)
        return [json.loads(line) for line in output.splitlines() if line.startswith('{')]

    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        job = pool.submit(turn)
        try:
            assert replay.inflight_started.wait(timeout=8)
            admin('providers/inflight-source', method='DELETE')
            admin('providers', {**source, 'base':base + '/inflight-new/v1'})
        finally:
            replay.inflight_release.set()
        job.result(timeout=40)
    rows = [r for r in admin('gateway')['requests'] if r['model'] == group]
    assert any(r['provider'] == 'inflight-backup' and r['attempts'][0]['status'] == 403 for r in rows)
    prior_backup = replay.pause_plan_calls
    events = turn()
    assert any(e.get('type') == 'text' and e['part']['text'] == 'new-source-ok' for e in events)
    assert replay.pause_plan_calls == prior_backup
    print('OpenCode in-flight rebind: old 403 fell back; next turn used the replacement free endpoint', flush=True)


def check_probe_slots_replay(client, env, project, replay, admin):
    model = 'probe-model'
    admin('providers', {'id':'probe-source', 'base':f'http://127.0.0.1:{replay.server_port}/probe-slots/v1',
                        'models':[model], 'kind':'subscription'})
    admin('agents/opencode/enable', {'on':True})
    endpoint = admin('gateway')['endpoint']
    with open_chat_stream(endpoint, model) as direct, concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        job = pool.submit(run_cli, client, ['--pure','run','--format','json','--model',f'aubridge/{model}',
                                           '--dir',str(project),'--','Reply briefly.'], env, project)
        try:
            deadline = time.monotonic() + 8
            while replay.probe_stream_calls < 2 and not job.done() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert replay.probe_stream_calls == 2
            busy = admin('providers/probe-source/check', {})
            assert busy['status'] == 'provider-busy' and busy['upstream_status'] is None, busy
            assert replay.probe_calls == 0
        finally:
            replay.queue_release.set()
        assert b'held-slot' in direct.read()
        events = [json.loads(line) for line in job.result(timeout=40).splitlines() if line.startswith('{')]
        assert any(e.get('type') == 'text' and e['part']['text'] == 'held-slot' for e in events)
    assert admin('providers/probe-source/check', {})['status'] == 'ok'
    assert replay.probe_calls == 1
    print('OpenCode probe slots: busy with zero probe traffic while occupied; probe succeeded after release')


def check_catalog_sync_replay(client, env, project, replay, admin, config, plugins):
    admin('agents/opencode/enable', {'on': True})

    def listed():
        return set(run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines())

    base = f'http://127.0.0.1:{replay.server_port}/edit-models/v1'
    assert 'aubridge/sync-literal' not in listed()
    source = {'id':'sync-custom', 'base':base, 'models':['sync-literal']}
    if sys.platform == 'darwin':
        source['key'] = 'isolated-catalog-fixture'
    admin('providers', source)
    if sys.platform == 'darwin':
        assert any(row['id'] == 'sync-custom' and row['key_set'] for row in admin('providers')['providers'])
    admin('model-groups', {'name':'sync-group', 'members':['sync-literal']})
    assert {'aubridge/sync-literal','aubridge/sync-group'} <= listed()
    output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model', 'aubridge/sync-group',
                              '--dir', str(project), '--', 'Reply briefly.'], env, project)
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    assert any(e.get('type') == 'text' and e['part']['text'] == 'model-edit-ok' for e in events)

    plugins.mkdir(parents=True, exist_ok=True)
    for name in ['sync-plugin', 'sync-auto']:
        (plugins / (name + '.json')).write_text(json.dumps({'id':name, 'version':'1',
            'provider':{'base':base,'auth':'bearer'},'free_models':[name]}))
    admin('plugins/sync-plugin/enable', {'on': True})
    assert 'aubridge/sync-plugin' in listed()
    admin('plugins/sync-plugin/enable', {'on': False})
    admin('plugins/auto', {'on': True})
    assert 'aubridge/sync-auto' in listed() and 'aubridge/sync-plugin' not in listed()
    admin('plugins/sync-auto/enable', {'on': False})
    admin('model-groups/sync-group', method='DELETE')
    admin('providers/sync-custom', method='DELETE')
    assert not {'aubridge/sync-literal','aubridge/sync-group','aubridge/sync-auto'} & listed()
    if sys.platform == 'darwin':
        missing = subprocess.run(['/usr/bin/security', 'find-generic-password', '-s', 'aubridge', '-a', 'sync-custom'],
                                 env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        assert missing.returncode == 44  # errSecItemNotFound

    owned = config.read_text()
    edited = owned.replace('"name":"aubridge"', '"name":"Fixture user edit"', 1)
    assert edited != owned
    config.write_text(edited)
    admin('providers', {'id':'sync-drift', 'base':base, 'models':['sync-drift']})
    assert config.read_text() == edited
    config.write_text(owned)
    admin('providers/sync-drift', method='DELETE')
    preview = admin('agents/opencode/disconnect-preview')
    assert not preview['blocked']
    admin('agents/opencode/enable', {'on':False, 'preview':preview})
    restored = config.read_text()
    assert '"name": "Original"' in restored and '// fixture comment inside' in restored
    assert '// fixture comment outside' in restored
    print('OpenCode catalog sync: source, group and plugin changes discovered; drift and original configuration preserved')


def prepare_replay_keychain(home, env):
    library = home / 'Library'
    (library / 'Preferences').mkdir(parents=True)
    (library / 'Keychains').mkdir()
    keychain = str((library / 'Keychains/replay.keychain-db').resolve())
    for command in [
        ['create-keychain', '-p', '', keychain],
        ['list-keychains', '-d', 'user', '-s', keychain],
        ['default-keychain', '-d', 'user', '-s', keychain],
    ]:
        subprocess.run(['/usr/bin/security', *command], env=env, check=True)
    default = subprocess.check_output(['/usr/bin/security', 'default-keychain', '-d', 'user'], env=env, text=True)
    assert default.strip().strip('"') == keychain


def check(gateway, client, root, temp, replay, discovered_model, replay_mode):
    config_dir = temp / 'config'
    config = config_dir / 'opencode/opencode.jsonc'
    config.parent.mkdir(parents=True)
    config.write_text(INITIAL)
    project = temp / 'project'
    project.mkdir()
    env = {'PATH': os.environ['PATH'], 'HOME': str(temp)}
    for name in ('CONFIG', 'CACHE', 'DATA', 'STATE'):
        env[f'XDG_{name}_HOME'] = str(temp / name.lower())
    gateway_env = {**os.environ, **env, 'PLUGINS_DIR': str(temp / 'gateway/plugins'),
                   'CLAUDE_CONFIG_DIR': str(temp / 'claude'), 'CODEX_HOME': str(temp / 'codex'),
                   'OPENCODE_CONFIG': str(config), 'KEY_REPLAY_429': 'fixture'}
    for name in ('https_proxy', 'http_proxy', 'all_proxy', 'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY'):
        gateway_env.pop(name, None)
    if replay_mode:
        gateway_env = {key: value for key, value in gateway_env.items() if not key.startswith('KEY_')}
    if sys.platform == 'darwin' and replay_mode in ('queued-rebind', 'inflight-rebind', 'catalog-sync'):
        prepare_replay_keychain(temp, gateway_env)
    if replay_mode in ('context', 'context-failover'):
        seed_context_replay(pathlib.Path(gateway_env['PLUGINS_DIR']), replay,
                            reject_first=replay_mode == 'context-failover')
    for name in ('MODELS_FETCH', 'AUTOUPDATE', 'DEFAULT_PLUGINS', 'EXTERNAL_SKILLS',
                 'LSP_DOWNLOAD', 'CLAUDE_CODE', 'SHARE'):
        env[f'OPENCODE_DISABLE_{name}'] = '1'

    with (temp / 'gateway.log').open('w+') as log, subprocess.Popen(
            [gateway], cwd=temp if replay_mode else root, env=gateway_env, stdout=log, stderr=log) as process:
        try:
            session_file = temp / 'gateway/gateway-session.json'
            for _ in range(100):
                if session_file.is_file() or process.poll() is not None:
                    break
                time.sleep(0.05)
            session = json.loads(session_file.read_text())
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

            def admin(path, body=None, method=None):
                payload = json.dumps(body).encode() if body is not None else None
                request = urllib.request.Request(session['endpoint'] + '/admin/' + path, payload,
                                                 {'Authorization': 'Bearer ' + session['admin_token'],
                                                  'Content-Type': 'application/json'}, method=method)
                try:
                    with opener.open(request, timeout=10) as response:
                        return json.load(response)
                except urllib.error.HTTPError as error:
                    raise RuntimeError(f'{path}: {error.code} {error.read(300)}') from None

            if replay_mode in ('context', 'context-failover'):
                check_context_replay(client, env, project, replay, admin, reject_first=replay_mode == 'context-failover')
                return
            if replay_mode in ('cooldown', 'dated-cooldown'):
                check_cooldown_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'embedded':
                check_embedded_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'pause':
                check_pause_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'resume':
                check_resume_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'free-only':
                check_free_only_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'model-edit':
                check_model_edit_replay(client, env, project, replay, admin)
                return
            if replay_mode in ('queued-edit', 'queued-rebind'):
                check_queued_edit_replay(client, env, project, replay, admin, rebind=replay_mode == 'queued-rebind')
                return
            if replay_mode == 'inflight-rebind':
                check_inflight_rebind_replay(client, env, project, replay, admin)
                return
            if replay_mode == 'catalog-sync':
                check_catalog_sync_replay(client, env, project, replay, admin, config,
                                          pathlib.Path(gateway_env['PLUGINS_DIR']))
                return
            if replay_mode == 'probe-slots':
                check_probe_slots_replay(client, env, project, replay, admin)
                return

            admin('providers', {'id': 'replay-429', 'base': f'http://127.0.0.1:{replay.server_port}/v1',
                                'models': ['replay-cooldown']})
            free_models = ['cohere/north-mini-code:free', 'nvidia/nemotron-3.5-lightning:free']
            if discovered_model:
                admin('agents/opencode/enable', {'on': True})
                before = run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
                assert f'aubridge/{discovered_model}' not in before
                directory = admin('providers/openrouter/catalog', {})
                assert directory['issue'] is None, directory['issue']
                assert any(model['id'] == discovered_model and model['tools'] for model in directory['models'])
                print(f"refreshed {len(directory['models'])} public free models", flush=True)
                free_models = [discovered_model]
            groups = {
                'validation-openrouter': ['kimi-k3', *free_models],
                'validation-kimi': ['kimi-k3', 'replay-cooldown'],
            }
            for group, members in groups.items():
                admin('model-groups', {'name': group, 'members': members, 'rule': 'free_first'})
            admin('agents/opencode/enable', {'on': True})
            models = run_cli(client, ['--pure', 'models', 'aubridge'], env, project).splitlines()
            assert all(f'aubridge/{group}' in models for group in groups), models
            print(f'discovered {len(models)} models including both validation groups', flush=True)
            cases = [('validation-openrouter', 'openrouter')]
            if discovered_model:
                assert f'aubridge/{discovered_model}' in models
            else:
                cases.append(('validation-kimi', 'kimi-code'))
            for group, source in cases:
                token = 'aubridge-' + secrets.token_hex(6)
                fixture = project / 'probe.txt'
                fixture.write_text(token + '\n')
                output = run_cli(client, ['--pure', 'run', '--format', 'json', '--model',
                                         f'aubridge/{group}', '--dir', str(project), '--',
                                         'Use the read tool to read probe.txt in the current project. Reply with its exact contents.'],
                                 env, project)
                events = []
                for line in output.splitlines():
                    try:
                        events.append(json.loads(line))
                    except ValueError:
                        pass
                kinds = [(event.get('type'), (event.get('part') or {}).get('tool')) for event in events]
                rows = [row for row in admin('gateway')['requests'] if row['model'] == group]
                used = [row for row in rows if row['provider'] == source and row['outcome'] == 'ok'
                        and row['input_tokens'] is not None and row['output_tokens'] is not None
                        and (not discovered_model or row['served_model'] == discovered_model)]
                print(group, 'events', kinds, 'decisions', [
                    (row['provider'], row['outcome'], row['input_tokens'], row['output_tokens'],
                     [(attempt['id'], attempt.get('reason')) for attempt in row['attempts']])
                    for row in rows], flush=True)
                assert any(event.get('type') == 'tool_use' and event['part'].get('tool') == 'read'
                           and event['part'].get('state', {}).get('status') == 'completed' for event in events), output[-1600:]
                assert any(event.get('type') == 'text' and token in event['part']['text'] for event in events), [
                    (event['part'].get('text'), event['part'].get('state')) for event in events
                    if event.get('type') in ('text', 'tool_use', 'error')]
                assert len(used) >= 2, rows
                if source == 'kimi-code':
                    assert any(attempt.get('reason') == 'account-quota-exhausted'
                               for row in used for attempt in row['attempts'])

            extra = '  // setting added after connection\n  "theme": "system",\n'
            managed = config.read_text()
            config.write_text(managed.replace('  "share": "disabled",\n', '  "share": "disabled",\n' + extra))
            preview = admin('agents/opencode/disconnect-preview')
            admin('agents/opencode/enable', {'on': False, 'preview': preview})
            assert config.read_text() == INITIAL.replace('  "share": "disabled",\n', '  "share": "disabled",\n' + extra)
            print('disconnect restored original JSONC provider bytes and retained the new setting', flush=True)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(5)
                except subprocess.TimeoutExpired:
                    process.kill()


parser = argparse.ArgumentParser(description='Exercise OpenCode wiring and tool calls in an isolated profile. Run from the aubridge root.')
parser.add_argument('gateway', help='path to the aubridged binary')
parser.add_argument('client', help='path to the opencode binary')
mode = parser.add_mutually_exclusive_group()
mode.add_argument('--free-model', help='refresh the official directory and verify this model is newly discovered')
mode.add_argument('--context-replay', dest='replay_mode', action='store_const', const='context',
                  help='verify context routing using local replay and no upstream keys')
mode.add_argument('--context-failover-replay', dest='replay_mode', action='store_const', const='context-failover',
                  help='verify a confirmed context rejection falls back to a declared wider window')
mode.add_argument('--cooldown-replay', dest='replay_mode', action='store_const', const='cooldown',
                  help='verify OpenCode recovers from a local 429 and cooldown')
mode.add_argument('--dated-cooldown-replay', dest='replay_mode', action='store_const', const='dated-cooldown',
                  help='verify OpenCode waits for an absolute Retry-After date')
mode.add_argument('--embedded-replay', dest='replay_mode', action='store_const', const='embedded',
                  help='verify OpenCode fails over after an error in a local 200 stream')
mode.add_argument('--pause-replay', dest='replay_mode', action='store_const', const='pause',
                  help='verify pause and resume of a local fallback source')
mode.add_argument('--routing-resume-replay', dest='replay_mode', action='store_const', const='resume',
                  help='verify a source blocked by an upstream 403 can rejoin after an explicit resume')
mode.add_argument('--free-only-replay', dest='replay_mode', action='store_const', const='free-only',
                  help='verify a local free-only group in OpenCode')
mode.add_argument('--model-edit-replay', dest='replay_mode', action='store_const', const='model-edit',
                  help='verify OpenCode discovers and uses a model after editing a custom source')
mode.add_argument('--queued-edit-replay', dest='replay_mode', action='store_const', const='queued-edit',
                  help='verify an OpenCode request rechecks the selected model after waiting for a slot')
mode.add_argument('--queued-rebind-replay', dest='replay_mode', action='store_const', const='queued-rebind',
                  help='verify an OpenCode request rechecks the upstream endpoint after waiting for a slot')
mode.add_argument('--inflight-rebind-replay', dest='replay_mode', action='store_const', const='inflight-rebind',
                  help='verify an old endpoint cannot disable a replacement after the request was sent')
mode.add_argument('--catalog-sync-replay', dest='replay_mode', action='store_const', const='catalog-sync',
                  help='verify model discovery changes without re-enabling an OpenCode connection')
mode.add_argument('--probe-slots-replay', dest='replay_mode', action='store_const', const='probe-slots',
                  help='verify a connection test cannot exceed the subscription slots')
options = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='aubridge-opencode-') as directory, \
        http.server.ThreadingHTTPServer(('127.0.0.1', 0), Replay) as replay:
    replay.context_calls = []
    replay.cooldown_calls = []
    replay.dated_cooldown = options.replay_mode == 'dated-cooldown'
    replay.embedded_calls = []
    replay.pause_fail_free = False
    replay.pause_plan_calls = 0
    replay.resume_free_calls = 0
    replay.edited_model_calls = []
    replay.queued_model_calls = 0
    replay.probe_calls = 0
    replay.probe_stream_calls = 0
    replay.rebound_calls = 0
    replay.queue_release = threading.Event()
    replay.inflight_started = threading.Event()
    replay.inflight_release = threading.Event()
    threading.Thread(target=replay.serve_forever, daemon=True).start()
    try:
        check(options.gateway, options.client, pathlib.Path.cwd(), pathlib.Path(directory).resolve(), replay,
              options.free_model, options.replay_mode)
    finally:
        replay.shutdown()
