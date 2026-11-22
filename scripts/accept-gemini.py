"""Source-level real Gemini CLI -> fixed aubridged -> ChatGPT tool acceptance.

No local provider replay. Uses a private copy of authorized Codex auth only.
The private HOME contains no copied Google credentials. CLI output stays private.
This source-mode gate is not distribution, package or installation acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import uuid


def private_write(path, data):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), 'w') as stream:
        stream.write(data)


def fingerprint(paths):
    return {str(path): hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None for path in paths}


def collect_run_evidence(admin, ledger_path, session_id, output, receipt):
    # This runs before assertions and before the temporary HOME is removed.
    # Never store bodies, error messages, credentials, filesystem paths or SQL dumps.
    evidence = {'session_id': session_id, 'gateway': [], 'decisions': [], 'usage': [], 'collection_errors': []}
    fields = ('request_id', 'agent', 'session_id', 'model', 'provider', 'served_model',
              'outcome', 'input_tokens', 'output_tokens', 'first_content_ms', 'duration_ms')
    attempt_fields = ('id', 'model', 'kind', 'status', 'reason', 'duration_ms', 'chosen')

    def note(stage, error):
        item = {'stage': stage, 'type': type(error).__name__}
        status = getattr(error, 'code', None)
        if isinstance(status, int):
            item['status'] = status
        evidence['collection_errors'].append(item)

    def safe_row(row):
        result = {key: row[key] for key in fields if key in row}
        attempts = row.get('attempts', [])
        if isinstance(attempts, str):
            try:
                attempts = json.loads(attempts)
            except (ValueError, TypeError) as error:
                note('attempts-json', error)
                attempts = []
        if isinstance(attempts, list):
            result['attempts'] = [{key: attempt[key] for key in attempt_fields if key in attempt}
                                  for attempt in attempts if isinstance(attempt, dict)]
        error = row.get('error')
        if isinstance(error, dict):
            result['error'] = {key: error[key] for key in ('type', 'code', 'status') if key in error}
        return result

    try:
        rows = admin('gateway?session_id=' + session_id).get('requests', [])
        evidence['gateway'] = [safe_row(row) for row in rows
                               if row.get('session_id') == session_id and row.get('agent') == 'gemini']
    except Exception as error:
        note('gateway', error)
    request_ids = {row['request_id'] for row in evidence['gateway'] if row.get('request_id')}
    try:
        if ledger_path.is_file():
            with sqlite3.connect(f'file:{ledger_path}?mode=ro', uri=True, timeout=1) as ledger:
                ledger.row_factory = sqlite3.Row
                # Session scope establishes a request-id whitelist even if the admin read failed.
                rows = ledger.execute('SELECT request_id,agent,session_id,model,provider,served_model,outcome,input_tokens,output_tokens,first_content_ms,duration_ms,attempts FROM decisions WHERE session_id=? AND agent=?', (session_id, 'gemini'))
                evidence['decisions'] = [safe_row(dict(row)) for row in rows]
                request_ids.update(row['request_id'] for row in evidence['decisions'] if row.get('request_id'))
                for request_id in sorted(request_ids):
                    rows = ledger.execute('SELECT d.request_id,u.agent,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting FROM usage u JOIN usage_details d ON d.usage_rowid=u.rowid WHERE d.request_id=? AND u.agent=?', (request_id, 'gemini'))
                    evidence['usage'].extend(dict(row) for row in rows)
    except Exception as error:
        note('ledger', error)
    receipt['run_evidence'] = evidence
    try:
        private_write(output / 'run-evidence.json', json.dumps(evidence, indent=2))
    except Exception as error:
        note('evidence-write', error)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gateway', required=True)
    parser.add_argument('--client', required=True)
    parser.add_argument('--codex-auth', required=True, help='authorized source auth.json, read only')
    parser.add_argument('--expected-commit', required=True)
    parser.add_argument('--expected-tree', required=True, help='tree pinned by the independent daemon build receipt')
    parser.add_argument('--expected-binary-sha256', required=True, help='SHA-256 pinned by the independent daemon build receipt')
    parser.add_argument('--output', required=True)
    parser.add_argument('--select-model', help='explicit model chosen only through the product agent model API; omit for default AUTO')
    args = parser.parse_args()
    selected_model = args.select_model if args.select_model is not None else 'aubridge'
    assert selected_model, 'selected model must be nonempty'
    gateway, client = str(Path(args.gateway).resolve()), str(Path(args.client).resolve())
    output = Path(args.output).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    user_gemini = Path.home() / '.gemini'
    watched = [user_gemini / name for name in ('settings.json', '.env', 'oauth_creds.json', 'google_accounts.json')]
    auth_source = Path(args.codex_auth).resolve()
    watched.append(auth_source)
    before = fingerprint(watched)
    harness = Path(__file__).resolve()
    harness_revision = subprocess.check_output(['git', '-C', str(harness.parent.parent), 'rev-parse', 'HEAD'], text=True).strip()
    receipt = {'scope': 'source-level real Gemini CLI / fixed source gateway / ChatGPT; not package or installation acceptance', 'evidence_mode': 'source', 'status': 'FAIL', 'gateway_sha256': hashlib.sha256(Path(gateway).read_bytes()).hexdigest(), 'harness_git_revision': harness_revision, 'harness_sha256': hashlib.sha256(harness.read_bytes()).hexdigest(), 'expected_daemon': {'git_revision': args.expected_commit, 'git_tree': args.expected_tree, 'sha256': args.expected_binary_sha256, 'mode': 'source', 'source_verified': False}, 'user_config_before': before}
    receipt['model_selection'] = {'mode': 'explicit' if args.select_model is not None else 'default', 'requested_model': selected_model}
    try:
        assert len(args.expected_binary_sha256) == 64 and all(char in '0123456789abcdef' for char in args.expected_binary_sha256), 'expected binary digest must come from the independent build receipt'
        assert receipt['gateway_sha256'] == args.expected_binary_sha256, 'fixed daemon SHA-256 mismatch before launch'
        with tempfile.TemporaryDirectory(prefix='aubridge-gemini-real-') as temporary:
            root = Path(temporary)
            project, home = root / 'workspace', root / 'home'
            project.mkdir(mode=0o700)
            home.mkdir()
            plugins = root / 'gateway/plugins'
            plugins.mkdir(parents=True)
            codex = root / 'codex'
            codex.mkdir(mode=0o700)
            original_auth = json.loads(auth_source.read_text())
            tokens = original_auth.get('tokens', {})
            assert isinstance(tokens, dict) and isinstance(tokens.get('access_token'), str) and tokens['access_token'], 'source has no supported Codex access token'
            copied_tokens = {key: tokens[key] for key in ('access_token', 'id_token', 'account_id') if isinstance(tokens.get(key), str)}
            copied_tokens['refresh_token'] = ''
            private_write(codex / 'auth.json', json.dumps({'tokens': copied_tokens}))
            del original_auth, tokens, copied_tokens
            receipt['credential_copy'] = {'scope': 'temporary private CODEX_HOME', 'share_login': False, 'refresh_token_cleared': True}
            private_write(root / 'gateway/config.json', json.dumps({'share_login': False, 'claude_cli_enabled': False, 'chatgpt_login_source': 'codex'}))
            # Gateway inherits upstream credentials only in its environment, never argv/output.
            daemon_env = {**os.environ, 'HOME': str(home), 'GEMINI_CLI_HOME': str(home), 'PLUGINS_DIR': str(plugins), 'CLAUDE_CONFIG_DIR': str(root / 'claude'), 'CODEX_HOME': str(root / 'codex'), 'AUBRIDGE_GEMINI_BIN': client}
            for key in list(daemon_env):
                if key.startswith(('GOOGLE_', 'GEMINI_', 'GCLOUD_', 'VERTEX_')) or key.lower() in ('http_proxy', 'https_proxy', 'all_proxy'):
                    daemon_env.pop(key)
            for kind in ('CONFIG', 'DATA', 'STATE', 'CACHE'):
                daemon_env[f'XDG_{kind}_HOME'] = str(root / kind.lower())
            with (output / 'gateway.log').open('w') as log:
                os.chmod(output / 'gateway.log', 0o600)
                daemon = subprocess.Popen([gateway], cwd=root, env=daemon_env, stdout=log, stderr=log, start_new_session=True)
                try:
                    session_path = root / 'gateway/gateway-session.json'
                    for _ in range(200):
                        if session_path.is_file() or daemon.poll() is not None:
                            break
                        time.sleep(.05)
                    session = json.loads(session_path.read_text())
                    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                    def admin(path, body=None):
                        data = json.dumps(body).encode() if body is not None else None
                        request = urllib.request.Request(session['endpoint'] + '/admin/' + path, data, {'Authorization': 'Bearer ' + session['admin_token'], 'Content-Type': 'application/json'})
                        with opener.open(request, timeout=45) as response:
                            return json.load(response)
                    build = admin('build')
                    receipt['build'] = build
                    receipt['daemon_git_revision'] = build.get('git_revision')
                    assert build.get('git_revision') == args.expected_commit and build.get('git_tree') == args.expected_tree, 'fixed daemon commit/tree mismatch'
                    assert build.get('pid') == daemon.pid, 'admin build PID does not match spawned daemon'
                    assert build.get('mode') == 'source' and build.get('source_verified') is False, 'this gate requires ordinary source-mode evidence'
                    assert 'gemini-native-ingress' in build.get('capabilities', []), 'fixed daemon lacks Gemini native ingress capability'
                    providers = admin('providers')['providers']
                    assert any(provider['id'] == 'chatgpt' and provider.get('ready') for provider in providers), 'private ChatGPT auth is not ready'
                    for provider in providers:
                        admin('providers/' + provider['id'] + '/pause', {'paused': provider['id'] != 'chatgpt'})
                    admin('agents/gemini/enable', {'on': True})
                    if args.select_model is not None:
                        admin('agents/gemini/model', {'model': selected_model})
                    nonce = 'gemini-real-' + secrets.token_hex(24)
                    (project / 'probe.txt').write_text(nonce + '\n')
                    allowed = {'PATH', 'LANG', 'LC_ALL', 'LC_CTYPE', 'NODE_EXTRA_CA_CERTS', 'SSL_CERT_FILE'}
                    cli_env = {key: value for key, value in os.environ.items() if key in allowed}
                    for kind in ('CONFIG', 'DATA', 'STATE', 'CACHE'):
                        cli_env[f'XDG_{kind}_HOME'] = str(root / kind.lower())
                    cli_env.update(HOME=str(home), GEMINI_CLI_HOME=str(home), NO_COLOR='1', CI='true', TMPDIR=str(root), GEMINI_CLI_TRUST_WORKSPACE='true')
                    session_id = str(uuid.uuid4())
                    cli_env['GEMINI_CLI_CUSTOM_HEADERS'] = 'x-session-id: ' + session_id
                    receipt['session_source'] = 'caller-explicit --session-id and x-session-id'
                    receipt['session_id'] = session_id
                    command = [client, '--session-id', session_id, '--prompt', 'Use the read_file tool to read probe.txt in the current directory. Reply with its exact contents only.', '--output-format', 'stream-json', '--approval-mode', 'default']
                    cli = subprocess.Popen(command, cwd=project, env=cli_env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
                    try:
                        stdout, stderr = cli.communicate(timeout=180)
                    except subprocess.TimeoutExpired:
                        os.killpg(cli.pid, signal.SIGKILL)
                        stdout, stderr = cli.communicate()
                        private_write(output / 'cli.stdout', stdout.decode(errors='replace'))
                        private_write(output / 'cli.stderr', stderr.decode(errors='replace'))
                        try:
                            collect_run_evidence(admin, root / 'gateway/ledger.sqlite', session_id, output, receipt)
                        except Exception as error:
                            receipt['evidence_collection_error'] = {'type': type(error).__name__}
                        raise AssertionError('Gemini CLI timed out') from None
                    finally:
                        if cli.poll() is None:
                            os.killpg(cli.pid, signal.SIGKILL)
                            cli.wait()
                    try:
                        collect_run_evidence(admin, root / 'gateway/ledger.sqlite', session_id, output, receipt)
                    except Exception as error:
                        receipt['evidence_collection_error'] = {'type': type(error).__name__}
                    private_write(output / 'cli.stdout', stdout.decode(errors='replace'))
                    private_write(output / 'cli.stderr', stderr.decode(errors='replace'))
                    receipt['cli_exit_code'] = cli.returncode
                    events = [json.loads(line) for line in stdout.decode().splitlines() if line.startswith('{')]
                    receipt['event_types'] = [event.get('type') for event in events]
                    assert any(event.get('type') == 'init' and event.get('session_id') == session_id and event.get('model') == selected_model for event in events), 'CLI init selected-model/session mismatch'
                    uses = [event for event in events if event.get('type') == 'tool_use' and event.get('tool_name') == 'read_file']
                    successful_tools = [event for event in events if event.get('type') == 'tool_result' and event.get('status') == 'success' and any(event.get('tool_id') == use.get('tool_id') for use in uses)]
                    last_tool = max((index for index, event in enumerate(events) if event.get('type') == 'tool_result'), default=-1)
                    final = ''.join(event.get('content', '') for event in events[last_tool + 1:] if event.get('type') == 'message' and event.get('role') == 'assistant')
                    assert cli.returncode == 0 and successful_tools and final.strip() == nonce, 'CLI did not complete builtin read_file and exact nonce reply'
                    assert any(event.get('type') == 'result' and event.get('status') == 'success' for event in events), 'missing successful CLI terminal result'
                    assert not any(event.get('type') == 'error' for event in events), 'CLI error event'
                    rows = [row for row in admin('gateway')['requests'] if row.get('agent') == 'gemini' and row.get('model') == selected_model and row.get('outcome') == 'ok']
                    rows = [row for row in rows if row.get('provider') == 'chatgpt' and row.get('served_model') and row['served_model'] != 'aubridge' and row.get('request_path') == '/v1beta/models/' + selected_model + ':streamGenerateContent']
                    assert len({row.get('request_id') for row in rows}) >= 2, 'missing distinct real request IDs'
                    for row in rows:
                        assert any(attempt.get('chosen') and attempt.get('id') == 'chatgpt' and attempt.get('model') == row['served_model'] for attempt in row.get('attempts', [])), 'chosen actual model mismatch'
                    assert len(rows) >= 2 and all(row.get('request_id') and row.get('session_id') for row in rows), 'missing multi-turn request identity/session metadata'
                    sessions = {row['session_id'] for row in rows}
                    assert sessions == {session_id}, 'tool continuation lost explicit CLI session identity'
                    ledger_path = root / 'gateway/ledger.sqlite'
                    ledger_rows = []
                    for _ in range(60):
                        with sqlite3.connect(f'file:{ledger_path}?mode=ro', uri=True) as ledger:
                            ledger.row_factory = sqlite3.Row
                            ledger_rows = [dict(row) for row in ledger.execute('SELECT request_id,agent,provider,model,served_model,outcome,session_id,input_tokens,output_tokens FROM decisions WHERE agent=?', ('gemini',))]
                        if {row['request_id'] for row in rows} <= {row['request_id'] for row in ledger_rows}:
                            break
                        time.sleep(.1)
                    for row in rows:
                        matching = next((item for item in ledger_rows if item['request_id'] == row['request_id']), None)
                        assert matching and all(matching[key] == row[key] for key in ('agent', 'provider', 'model', 'served_model', 'outcome', 'session_id')), 'ledger/dashboard mismatch'
                        assert matching['input_tokens'] is not None and matching['output_tokens'] is not None, 'usage missing'
                    usage_rows = []
                    with sqlite3.connect(f'file:{ledger_path}?mode=ro', uri=True) as ledger:
                        ledger.row_factory = sqlite3.Row
                        for row in rows:
                            usage = [dict(item) for item in ledger.execute('SELECT d.request_id,u.agent,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting FROM usage u JOIN usage_details d ON d.usage_rowid=u.rowid WHERE d.request_id=?', (row['request_id'],))]
                            assert any(item['agent'] == 'gemini' and item['provider'] == 'chatgpt' and item['model'] == row['served_model'] and item['input_tokens'] == row['input_tokens'] and item['output_tokens'] == row['output_tokens'] for item in usage), 'actual usage ledger mismatch'
                            usage_rows.extend(usage)
                    receipt['usage'] = usage_rows
                    receipt['requests'] = [{key: row.get(key) for key in ('request_id','agent','provider','model','served_model','outcome','session_id','input_tokens','output_tokens','request_path','attempts')} for row in rows]
                    preview = admin('agents/gemini/disconnect-preview')
                    assert not preview['blocked']
                    admin('agents/gemini/enable', {'on': False, 'preview': preview})
                    settings_path = home / '.gemini/settings.json'
                    restored_settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
                    assert 'name' not in restored_settings.get('model', {}) and 'selectedType' not in restored_settings.get('security', {}).get('auth', {}), 'managed Gemini settings not removed'
                    restored_aliases = restored_settings.get('modelConfigs', {}).get('customAliases', {})
                    assert 'aubridge' not in restored_aliases and selected_model not in restored_aliases, 'managed selected alias not removed'
                    env_path = home / '.gemini/.env'
                    restored_env = env_path.read_text() if env_path.exists() else ''
                    assert 'GOOGLE_GEMINI_BASE_URL=' not in restored_env and 'GEMINI_API_KEY=' not in restored_env, 'managed Gemini env not removed'
                finally:
                    if daemon.poll() is None:
                        os.killpg(daemon.pid, signal.SIGINT)
                        try:
                            daemon.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(daemon.pid, signal.SIGKILL)
                            daemon.wait()
        receipt['private_auth_removed'] = not (codex / 'auth.json').exists()
        assert receipt['private_auth_removed'], 'private auth copy not removed'
        receipt['status'] = 'PASS'
    finally:
        receipt['user_config_after'] = fingerprint(watched)
        receipt['user_config_unchanged'] = receipt['user_config_after'] == before
        if not receipt['user_config_unchanged']:
            receipt['status'] = 'FAIL'
        private_write(output / 'receipt.json', json.dumps(receipt, indent=2))
    assert receipt['status'] == 'PASS'
    print(json.dumps({'status': receipt['status'], 'scope': receipt['scope'], 'requests': len(receipt['requests']), 'receipt': str(output / 'receipt.json')}))


if __name__ == '__main__':
    main()
