"""One real first-task job through the production admin API, in a private source daemon.

This does not invoke the CLI directly and does not prove package/installation readiness.
Only the selected CLI runs, once. Credentials are copied read-only without refresh tokens.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request


_spec = importlib.util.spec_from_file_location('gemini_accept_helpers', Path(__file__).with_name('accept-gemini.py'))
_helpers = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_helpers)
private_write = _helpers.private_write
fingerprint = _helpers.fingerprint
AGENTS = {'claude-code': 'AUBRIDGE_CLAUDE_BIN', 'codex': 'AUBRIDGE_CODEX_BIN',
          'opencode': 'AUBRIDGE_OPENCODE_BIN', 'gemini': 'AUBRIDGE_GEMINI_BIN'}


def require(condition, code):
    if not condition:
        raise AssertionError(code)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def stop_owned(process):
    if process is None or process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGINT)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def verify_job(admin, ledger_path, agent, job, output, receipt):
    evidence = job.get('evidence') or {}
    session = evidence.get('session_id')
    requests = evidence.get('requests', [])
    require(isinstance(session, str) and session, 'missing_native_session')
    require(isinstance(requests, list) and 2 <= len(requests) <= 64, 'missing_two_job_requests')
    ids = [row.get('request_id') for row in requests]
    require(all(isinstance(value, str) and value for value in ids) and len(set(ids)) == len(ids),
            'duplicate_or_missing_request_ids')
    require(evidence.get('tool_result_matched') is True and evidence.get('final_exact') is True,
            'file_task_not_proven')
    query = urllib.parse.urlencode({'agent': agent, 'session_id': session})
    rows = admin('gateway?' + query).get('requests', [])
    gateway = {row.get('request_id'): row for row in rows
               if row.get('agent') == agent and row.get('session_id') == session}
    joined = []
    with sqlite3.connect(ledger_path.as_uri() + '?mode=ro', uri=True, timeout=2) as ledger:
        ledger.row_factory = sqlite3.Row
        for request in requests:
            request_id = request['request_id']
            row = gateway.get(request_id)
            require(row and row.get('outcome') == 'ok', 'gateway_request_not_successful')
            require(request.get('provider') == row.get('provider') == 'chatgpt'
                    and request.get('served_model') == row.get('served_model'), 'actual_source_model_mismatch')
            chosen = [item for item in row.get('attempts', []) if item.get('chosen') is True]
            require(len(chosen) == 1 and chosen[0].get('id') == 'chatgpt'
                    and chosen[0].get('model') == row.get('served_model'), 'chosen_attempt_mismatch')
            decisions = [dict(item) for item in ledger.execute(
                'SELECT request_id,agent,session_id,provider,served_model,outcome,input_tokens,output_tokens '
                'FROM decisions WHERE request_id=? AND agent=? AND session_id=?', (request_id, agent, session))]
            require(len(decisions) == 1, 'decision_join_not_unique')
            decision = decisions[0]
            require(all(decision[key] == row.get(key) for key in
                        ('agent', 'session_id', 'provider', 'served_model', 'outcome')), 'decision_gateway_mismatch')
            usage = [dict(item) for item in ledger.execute(
                'SELECT x.request_id,u.agent,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting '
                'FROM usage u JOIN usage_details x ON x.usage_rowid=u.rowid '
                'WHERE x.request_id=? AND u.agent=? AND u.provider=? AND u.model=?',
                (request_id, agent, row['provider'], row['served_model']))]
            require(len(usage) == 1, 'actual_usage_join_not_unique')
            actual = usage[0]
            require(all(actual[key] == request.get(key) == decision[key]
                        for key in ('input_tokens', 'output_tokens')), 'actual_usage_tokens_mismatch')
            joined.append({'request_id': request_id, 'agent': agent, 'session_id': session,
                           'provider': actual['provider'], 'served_model': actual['model'],
                           'input_tokens': actual['input_tokens'], 'output_tokens': actual['output_tokens'],
                           'accounting': actual['accounting'], 'chosen_verified': True})
    receipt['verified_requests'] = joined
    if agent == 'gemini':
        _helpers.collect_run_evidence(admin, ledger_path, session, output, receipt)
        require(not receipt['run_evidence']['collection_errors'], 'evidence_collection_failed')
    else:
        private_write(output / 'run-evidence.json', json.dumps({'session_id': session, 'requests': joined}, indent=2))


def collect_failure_evidence(admin, ledger_path, output, receipt):
    # This daemon/data directory belongs only to this one attempted job. No native
    # session attribution is inferred, and these rows can never establish PASS.
    fields = ('request_id', 'agent', 'session_id', 'provider', 'served_model',
              'outcome', 'input_tokens', 'output_tokens', 'duration_ms')
    evidence = {'scope': 'single-private-job diagnostic', 'native_identity_confirmed': False,
                'candidate_pid': receipt.get('build', {}).get('pid'), 'job_id': receipt.get('job_id'),
                'gateway': [], 'decisions': [], 'usage': [], 'collection_errors': []}

    def safe(row):
        result = {key: row[key] for key in fields if key in row}
        error = row.get('error')
        if isinstance(error, dict):
            result['error'] = {key: error[key] for key in ('type', 'code', 'status') if key in error}
        return result

    try:
        # Terminal/cleanup may precede the final write-behind settlement briefly.
        for attempt in range(3):
            if attempt:
                time.sleep(.15)
            rows, cursor, seen = {}, None, set()
            for _ in range(32):
                page = admin('gateway' + ('?' + urllib.parse.urlencode({'before': cursor}) if cursor else ''), timeout=2)
                require(page.get('history_status') == 'available' and isinstance(page.get('requests'), list),
                        'diagnostic_history_unavailable')
                for row in page['requests']:
                    if isinstance(row, dict) and isinstance(row.get('request_id'), str):
                        rows[row['request_id']] = safe(row)
                cursor = page.get('next')
                if not cursor:
                    break
                require(isinstance(cursor, str) and cursor not in seen, 'diagnostic_cursor_repeated')
                seen.add(cursor)
            else:
                raise RuntimeError('diagnostic_page_limit')
            evidence['gateway'] = list(rows.values())
            evidence['decisions'], evidence['usage'] = [], []
            require(ledger_path.is_file(), 'diagnostic_ledger_unavailable')
            with sqlite3.connect(ledger_path.as_uri() + '?mode=ro', uri=True, timeout=.5) as ledger:
                ledger.row_factory = sqlite3.Row
                for request_id in rows:
                    decisions = ledger.execute(
                        'SELECT request_id,agent,session_id,provider,served_model,outcome,input_tokens,output_tokens,duration_ms '
                        'FROM decisions WHERE request_id=?', (request_id,))
                    evidence['decisions'].extend(safe(dict(row)) for row in decisions)
                    usage = ledger.execute(
                        'SELECT x.request_id,u.agent,u.provider,u.model AS served_model,u.input_tokens,u.output_tokens '
                        'FROM usage u JOIN usage_details x ON x.usage_rowid=u.rowid WHERE x.request_id=?', (request_id,))
                    evidence['usage'].extend(safe(dict(row)) for row in usage)
    except Exception as error:
        evidence['collection_errors'].append(type(error).__name__)
    receipt['failure_evidence'] = evidence
    try:
        private_write(output / 'failure-evidence.json', json.dumps(evidence, indent=2))
    except Exception as error:
        evidence['collection_errors'].append(type(error).__name__)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gateway', required=True)
    parser.add_argument('--client', required=True, help='explicit path to one official CLI; never invoked by this harness')
    parser.add_argument('--agent', required=True, choices=sorted(AGENTS))
    parser.add_argument('--codex-auth', required=True, help='authorized auth.json, read-only source')
    parser.add_argument('--expected-commit', required=True)
    parser.add_argument('--expected-tree', required=True)
    parser.add_argument('--expected-binary-sha256', required=True)
    parser.add_argument('--select-model', default='gpt-6.1-sol')
    parser.add_argument('--output', required=True, help='new or empty private evidence directory')
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(not any(output.iterdir()), 'output_directory_not_empty')
    os.chmod(output, 0o700)
    gateway, client, auth = (Path(value).resolve() for value in (args.gateway, args.client, args.codex_auth))
    home = Path.home()
    watched = [auth, home / '.codex/config.toml', home / '.codex/aubridge.config.toml', home / '.codex/auth.json',
               home / '.claude/settings.json', home / '.claude/.credentials.json',
               home / '.gemini/settings.json', home / '.gemini/.env', home / '.gemini/oauth_creds.json',
               home / '.gemini/google_accounts.json', home / '.config/opencode/opencode.json',
               home / '.config/opencode/opencode.jsonc', home / '.local/share/opencode/auth.json',
               home / '.aubridge/config.json']
    for variable, names in {'CODEX_HOME': ('config.toml', 'aubridge.config.toml', 'auth.json'),
                            'CLAUDE_CONFIG_DIR': ('settings.json', '.credentials.json')}.items():
        if os.environ.get(variable):
            watched.extend(Path(os.environ[variable]) / name for name in names)
    before = None
    script = Path(__file__).resolve()
    receipt = {'scope': 'one production private_probe job / fixed source daemon / real official CLI and ChatGPT; not installation acceptance',
               'status': 'FAIL', 'agent': args.agent, 'evidence_mode': 'source',
               'expected_daemon': {'git_revision': args.expected_commit, 'git_tree': args.expected_tree,
                                   'sha256': args.expected_binary_sha256, 'mode': 'source', 'source_verified': False},
               'harness_sha256': digest(script), 'helper_sha256': digest(script.with_name('accept-gemini.py')),
               'model_selection': args.select_model}
    daemon, admin, job_id, temporary = None, None, None, None
    stage = 'preflight'
    try:
        before = fingerprint(watched)
        receipt['user_config_before'] = before
        require(gateway.is_file() and os.access(gateway, os.X_OK), 'gateway_not_executable')
        require(client.is_file() and os.access(client, os.X_OK), 'client_not_executable')
        require(len(args.expected_commit) == 40 and len(args.expected_tree) == 40, 'unpinned_source_identity')
        require(len(args.expected_binary_sha256) == 64 and digest(gateway) == args.expected_binary_sha256,
                'fixed_binary_sha256_mismatch')
        receipt['gateway_sha256'], receipt['client_sha256'] = digest(gateway), digest(client)
        temporary = tempfile.TemporaryDirectory(prefix='aubridge-first-task-')
        root = Path(temporary.name)
        private_home, data, codex = root / 'home', root / 'gateway', root / 'codex'
        for directory in (private_home, data / 'plugins', codex):
            directory.mkdir(mode=0o700, parents=True)
        os.chmod(data, 0o700)
        tokens = json.loads(auth.read_text()).get('tokens', {})
        require(isinstance(tokens, dict) and isinstance(tokens.get('access_token'), str)
                and tokens['access_token'], 'authorized_codex_access_token_unavailable')
        copied = {key: tokens[key] for key in ('access_token', 'id_token', 'account_id') if isinstance(tokens.get(key), str)}
        copied['refresh_token'] = ''
        private_write(codex / 'auth.json', json.dumps({'tokens': copied}))
        del tokens, copied
        private_write(data / 'config.json', json.dumps({'share_login': False, 'claude_cli_enabled': False,
                                                      'chatgpt_login_source': 'codex'}))
        receipt['credential_copy'] = {'scope': 'private_CODEX_HOME', 'refresh_token_cleared': True,
                                      'share_login': False, 'claude_subscription_enabled': False}
        allowed = {'PATH', 'LANG', 'LC_ALL', 'LC_CTYPE', 'NODE_EXTRA_CA_CERTS', 'SSL_CERT_FILE'}
        env = {key: value for key, value in os.environ.items() if key in allowed}
        env.update(HOME=str(private_home), USERPROFILE=str(private_home), CODEX_HOME=str(codex),
                   CLAUDE_CONFIG_DIR=str(root / 'claude'), GEMINI_CLI_HOME=str(private_home),
                   PLUGINS_DIR=str(data / 'plugins'), TMPDIR=str(root), NO_PROXY='127.0.0.1,localhost,::1',
                   no_proxy='127.0.0.1,localhost,::1', **{AGENTS[args.agent]: str(client)})
        env['PATH'] = os.pathsep.join([str(Path(args.client).expanduser().absolute().parent),
                                      str(client.parent), env.get('PATH', os.defpath)])
        for kind in ('CONFIG', 'DATA', 'STATE', 'CACHE'):
            env['XDG_' + kind + '_HOME'] = str(root / kind.lower())
        stage = 'launch'
        log_path = output / 'gateway.log'
        with os.fdopen(os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as log:
            daemon = subprocess.Popen([str(gateway)], cwd=root, env=env, stdout=log, stderr=log, start_new_session=True)
        session_path = data / 'gateway-session.json'
        deadline = time.monotonic() + 15
        while not session_path.is_file() and daemon.poll() is None and time.monotonic() < deadline:
            time.sleep(.05)
        session = json.loads(session_path.read_text())
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def call(path, body=None, method=None, timeout=30):
            request = urllib.request.Request(session['endpoint'] + '/admin/' + path,
                json.dumps(body).encode() if body is not None else None,
                {'Authorization': 'Bearer ' + session['admin_token'], 'Content-Type': 'application/json'}, method=method)
            with opener.open(request, timeout=timeout) as response:
                return json.load(response)
        admin = call
        stage = 'build_identity'
        build = admin('build')
        receipt['build'] = build
        require(build.get('git_revision') == args.expected_commit and build.get('git_tree') == args.expected_tree
                and build.get('pid') == daemon.pid and build.get('mode') == 'source'
                and build.get('source_verified') is False, 'running_build_identity_mismatch')
        receipt['build_identity_verified'] = True
        stage = 'discovery'
        discovery = admin('agents/' + args.agent + '/discovery', {})['discovery']
        receipt['discovery'] = discovery
        require(discovery.get('status') == 'ready', 'cli_discovery_not_ready')
        stage = 'source_policy'
        providers = admin('providers')['providers']
        selected = next(row for row in providers if row['id'] == 'chatgpt')
        identity = selected['source_health']['cost']['connection_identity']
        require(selected.get('ready') and isinstance(identity, str) and identity, 'chatgpt_not_ready')
        for provider in providers:
            admin('providers/' + urllib.parse.quote(provider['id'], safe='') + '/pause',
                  {'paused': provider['id'] != 'chatgpt'})
        admin('settings', {'cost_intent': 'free_then_subscription', 'subscription_authorizations': [
            {'provider': 'chatgpt', 'basis': 'personal_channel', 'connection_identity': identity}]})
        settings = admin('settings')
        require(settings.get('cost_intent') == 'free_then_subscription' and settings.get('share_login') is False,
                'cost_policy_not_applied')
        stage = 'managed_wiring'
        admin('agents/' + args.agent + '/enable', {'on': True})
        admin('agents/' + args.agent + '/model', {'model': args.select_model})
        stage = 'file_job'
        started = admin('agents/' + args.agent + '/validation-jobs', {})
        job_id = started.get('job_id')
        require(job_id and started.get('agent_id') == args.agent and started.get('scope') == 'private_probe',
                'started_job_identity_mismatch')
        receipt['job_id'] = job_id
        deadline = time.monotonic() + 180
        while True:
            job = admin('agent-validation-jobs/' + urllib.parse.quote(job_id, safe=''))
            receipt['job'] = job
            require(job.get('job_id') == job_id and job.get('agent_id') == args.agent
                    and job.get('scope') == 'private_probe', 'observed_job_identity_mismatch')
            if job.get('status') not in ('queued', 'running'):
                break
            require(time.monotonic() < deadline and daemon.poll() is None, 'job_deadline_exceeded')
            time.sleep(.5)
        require(job.get('status') == 'passed', 'production_file_job_failed')
        require(job['evidence'].get('cli_version') == discovery.get('version')
                and job['evidence'].get('gateway_revision') == args.expected_commit, 'job_evidence_identity_mismatch')
        stage = 'request_decision_usage_join'
        verify_job(admin, data / 'ledger.sqlite', args.agent, job, output, receipt)
        summary = next(row for row in admin('agents')['agents'] if row['id'] == args.agent)['validation']
        receipt['validation_summary'] = summary
        require(summary.get('last_job_id') == job_id and summary.get('valid') is True
                and summary.get('status') == 'passed' and summary.get('scope') == 'private_probe', 'job_not_currently_valid')
        require(json.loads((data / 'config.json').read_text()).get('claude_cli_enabled') is False,
                'claude_subscription_unexpectedly_enabled')
        receipt['status'] = 'PASS'
    except Exception as error:
        receipt['failure'] = {'stage': stage, 'type': type(error).__name__}
        if isinstance(error, AssertionError):
            receipt['failure']['code'] = str(error)
        if isinstance(getattr(error, 'code', None), int):
            receipt['failure']['http_status'] = error.code
            try:
                failure = json.loads(error.read(4096)).get('error')
                code = failure.get('code') or failure.get('message') if isinstance(failure, dict) else failure
                if isinstance(code, str) and re.fullmatch(r'[a-z][a-z0-9_-]{0,119}', code):
                    receipt['failure']['api_code'] = code
            except Exception:
                pass
    finally:
        if admin and job_id:
            try:
                observed = admin('agent-validation-jobs/' + urllib.parse.quote(job_id, safe=''))
                if observed.get('status') in ('queued', 'running'):
                    admin('agent-validation-jobs/' + urllib.parse.quote(job_id, safe=''), method='DELETE')
                    until = time.monotonic() + 8
                    while observed.get('status') in ('queued', 'running') and time.monotonic() < until:
                        observed = admin('agent-validation-jobs/' + urllib.parse.quote(job_id, safe=''))
                        time.sleep(.1)
                    require(observed.get('status') not in ('queued', 'running'), 'job_cleanup_unconfirmed')
            except Exception as error:
                receipt['cleanup_error'] = {'type': type(error).__name__}
                receipt['status'] = 'FAIL'
        if receipt['status'] != 'PASS' and receipt.get('build_identity_verified') and admin and temporary is not None:
            collect_failure_evidence(admin, Path(temporary.name) / 'gateway/ledger.sqlite', output, receipt)
        try:
            stop_owned(daemon)
        except Exception as error:
            receipt['process_cleanup_error'] = {'type': type(error).__name__}
            receipt['status'] = 'FAIL'
        if temporary is not None:
            auth_copy = Path(temporary.name) / 'codex/auth.json'
            try:
                temporary.cleanup()
                receipt['private_auth_removed'] = not auth_copy.exists()
            except Exception as error:
                receipt['private_auth_removed'] = False
                receipt['private_cleanup_error'] = {'type': type(error).__name__}
            if receipt['private_auth_removed'] is not True:
                receipt['status'] = 'FAIL'
        try:
            receipt['user_config_after'] = fingerprint(watched)
            receipt['user_config_unchanged'] = before is not None and receipt['user_config_after'] == before
        except Exception as error:
            receipt['user_config_unchanged'] = False
            receipt['user_fingerprint_error'] = {'type': type(error).__name__}
        if not receipt['user_config_unchanged']:
            receipt['status'] = 'FAIL'
        private_write(output / 'receipt.json', json.dumps(receipt, indent=2))
    print(json.dumps({'status': receipt['status'], 'agent': args.agent,
                      'receipt': str(output / 'receipt.json'), 'scope': 'source/private_probe'}))
    return 0 if receipt['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
