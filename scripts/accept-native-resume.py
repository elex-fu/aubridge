#!/usr/bin/env python3
"""0114 operator-assisted acceptance; never starts an App, daemon, CLI or model.

Luna supplies a persistent private App's five-key manifest, then runs:
  prepare --manifest PRIVATE_JSON --agent gemini --client ABSOLUTE_OFFICIAL_CLI
  [execute the emitted seed command, using that same private environment]
  [CUA: in the private App's Agents page, check this CLI/version; keep wiring]
  seeded --manifest PRIVATE_JSON --agent gemini
  [CUA: refresh production Sessions, click Continue, enter continuation-prompt.txt]
  verify --manifest PRIVATE_JSON --agent gemini --observation PRIVATE_CUA_JSON

Repeat with claude-code, codex and opencode, in separate private workspaces.
This script writes challenge files and private hash/control receipts only. It
does not copy credentials, persist CLI output, manufacture native records, POST
resume, or count a terminal opening as successful continuation.

Manifest (all paths absolute; filled by Luna from the actual private App):
  private_root: a mode-0700 directory containing this manifest and the App
  environment: private HOME, CODEX_HOME, CLAUDE_CONFIG_DIR, GEMINI_CLI_HOME,
    XDG_*_HOME, OPENCODE_CONFIG/OPENCODE_DB as appropriate; no secrets
  workspace: this agent's existing, unique private project directory
  runtime: {gateway_access_file: product access JSON, ledger_path: product DB}
    Gemini additionally requires project_check_file: the unedited JSON response
    from the existing POST /admin/agents/gemini/project-check for this workspace,
    captured by Luna from the private App's real read-only project check. Never
    hand-author matched/trusted. Other agents do not need this field.
    Use the existing API's {"path": workspace} request with the private admin
    key kept in process memory; save its whole response as an owned 0600 file.
    Required response fields: status=matched, project_alias=workspace basename,
    env.trust=trusted, env.layer=home_gemini/project_plain/project_gemini.
  expected_build: {app_path, exe_sha256, git_revision, git_tree, pid, mode,
    source_verified}; values pinned to the current private App process/build
The private App must already have the intended managed connection/model. For
Gemini, confirm trust for only this private workspace through the official CLI
before prepare; this helper neither bypasses trust nor edits trustedFolders.

CUA sequence after seeded:
  1. Keep the private App and gateway running; do not recreate their HOME/data.
  2. Open the existing session diagnostics, "继续原对话" / "刷新原生会话".
  3. Select the exact agent and unique project alias, click "在原项目继续对话".
     Do not POST a handle separately or launch an independent resume command.
  4. Observe the product-created Terminal and original conversation context.
     The independent native record/request check proves the exact native ID;
     the terminal UI need not expose a UUID.
     Opening Terminal or a launched receipt alone is insufficient.
  5. Enter continuation-prompt.txt exactly; observe challenge read and result
     file tool completion, with no history/config/seed-file search.
  6. Fill observation-template.json truthfully without transcript/fact text.
     verify checks this observation together with the actual new file, native
     history change and exact native-ID request/decision/reported-usage joins.
Stop on the first real failure; never substitute a direct headless resume.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import secrets
import shlex
import sqlite3
import urllib.parse
import urllib.request
import uuid

AGENTS = ('gemini', 'claude-code', 'codex', 'opencode')
PATH_ENV = {'HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'GEMINI_CLI_HOME',
            'OPENCODE_CONFIG', 'OPENCODE_DB', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME',
            'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'TMPDIR'}
LIMIT = 8 * 1024 * 1024
SEED_PROMPT = ('Use your file reading tool to read native-resume-seed.txt in the '
               'current project. Remember its exact project marker for a later '
               'conversation turn. Reply with that marker only.')
CONTINUE_PROMPT = (
    'Continue this existing conversation. Recall the exact project marker you '
    'previously read. Do not open any history, session or configuration files, '
    'and do not search for the marker. Use your file tools to read '
    'native-resume-challenge.txt, then create native-resume-result.json in this '
    'project with exactly two JSON string fields: remembered_fact (the earlier '
    'project marker) and challenge (the exact new file contents). Do not include '
    'a trailing newline in either value. Reply only DONE after the file tool '
    'succeeds. If you do not remember the earlier marker, say so and stop.')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_bytes(path):
    with Path(path).open('rb') as stream:
        data = stream.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise ValueError('bounded_read_limit')
    return data


def load(path):
    return json.loads(read_bytes(path))


def write(path, value):
    path = Path(path)
    data = value if isinstance(value, str) else json.dumps(value, indent=2) + '\n'
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), 'w') as stream:
        stream.write(data)


def inside(root, path):
    path = Path(path).resolve()
    if not path.is_relative_to(root) or path == root:
        raise ValueError('path_outside_private_root')
    return path


def context(manifest_path, agent):
    manifest_path = Path(manifest_path).resolve()
    manifest = load(manifest_path)
    root = Path(manifest['private_root']).resolve(strict=True)
    if root == Path.home().resolve() or root.stat().st_mode & 0o077:
        raise ValueError('private_root_requires_0700')
    inside(root, manifest_path)
    workspace = inside(root, manifest['workspace'])
    if not workspace.is_dir():
        raise ValueError('workspace_missing')
    env = manifest['environment']
    if not isinstance(env, dict) or 'HOME' not in env or set(env) - PATH_ENV:
        raise ValueError('environment_must_only_contain_private_paths')
    for value in env.values():
        if not isinstance(value, str) or not Path(value).is_absolute() or any(ord(char) < 32 for char in value):
            raise ValueError('environment_requires_absolute_private_paths')
        inside(root, value)
    runtime = manifest['runtime']
    access = load(inside(root, runtime['gateway_access_file']))
    endpoint = urllib.parse.urlsplit(access['endpoint'])
    if endpoint.scheme != 'http' or endpoint.hostname not in ('127.0.0.1', 'localhost') or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment:
        raise ValueError('private_loopback_endpoint_required')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def admin(path):
        request = urllib.request.Request(access['endpoint'].rstrip('/') + '/admin/' + path,
                                         headers={'Authorization': 'Bearer ' + access['admin_token']})
        with opener.open(request, timeout=30) as response:
            return json.load(response)

    build = admin('build')
    expected = manifest['expected_build']
    for key in ('git_revision', 'git_tree', 'pid', 'mode', 'source_verified'):
        if key not in expected or build.get(key) != expected[key]:
            raise ValueError('private_app_build_mismatch_' + key)
    app = inside(root, expected['app_path'])
    info = plistlib.loads(read_bytes(app / 'Contents/Info.plist'))
    executable = inside(root, app / 'Contents/MacOS' / info['CFBundleExecutable'])
    binary_hash = hashlib.sha256()
    with executable.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            binary_hash.update(block)
    if binary_hash.hexdigest() != expected['exe_sha256']:
        raise ValueError('private_app_executable_changed')
    control = root / 'native-resume-acceptance' / agent
    control.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(control.parent, 0o700)
    return manifest, root, workspace, env, admin, build, control


def opencode_rows(connection, session_id):
    """V2 has composite part keys; both independent cursors share one budget."""
    columns = {table: {row[1]: row[5] for row in connection.execute('PRAGMA table_info(' + table + ')')}
               for table in ('session_message', 'session_part')}
    primary_keys = {table: {key: ordinal for key, ordinal in fields.items() if ordinal > 0} for table, fields in columns.items()}
    v2 = {'id', 'session_id', 'type', 'seq', 'data'} <= columns['session_message'].keys() and {'message_id', 'session_id', 'ordinal', 'type', 'data'} <= columns['session_part'].keys() and primary_keys['session_message'] == {'id': 1} and primary_keys['session_part'] == {'message_id': 1, 'ordinal': 2}
    legacy = {'id', 'session_id', 'data'} <= columns['session_message'].keys() and {'id', 'session_id', 'data'} <= columns['session_part'].keys()
    if not v2 and not legacy:
        raise ValueError('native_schema_unsupported')
    selections = [('message', 'id', 'seq', 'type', 'seq,id'), ('part', 'message_id', 'ordinal', 'type', 'message_id,ordinal')] if v2 else [('message', 'id', '0', "''", 'id'), ('part', 'id', '0', "''", 'id')]
    count = total = 0
    for kind, key, ordinal, row_type, order in selections:
        length = 'length(CAST(' + key + ' AS BLOB))+length(CAST(' + row_type + ' AS BLOB))+length(CAST(data AS BLOB))'
        bounded = lambda field: 'CASE WHEN ' + length + '<=?2 THEN ' + field + ' END'
        query = 'SELECT ' + bounded(key) + ",''," + ordinal + ',' + bounded(row_type) + ',' + bounded('substr(CAST(data AS BLOB),1,?2)') + ',' + length + ' FROM session_' + kind + ' WHERE session_id=?1 ORDER BY ' + order + ' LIMIT 4097'
        for key, parent, ordinal, row_type, data, length in connection.execute(query, (session_id, LIMIT)):
            if not isinstance(length, int) or length < 0:
                raise ValueError('native_schema_unsupported')
            count += 1
            total += length
            if count > 4096 or total > LIMIT:
                raise ValueError('native_message_limit')
            value = json.loads(data)
            yield v2, kind, key, parent, ordinal, row_type, data, value


def opencode_managed(connection, session_id, model, version):
    def managed(value):
        return isinstance(value, dict) and value.get('providerID') == 'aubridge' and isinstance(value.get('id'), str) and bool(value['id'])
    if model is not None:
        return managed(json.loads(model))
    assistants = 0
    if version != '2.2.5':
        return False
    for v2, kind, _, _, _, row_type, _, value in opencode_rows(connection, session_id):
        if not v2:
            return False
        if kind == 'message' and row_type == 'assistant':
            if not isinstance(value, dict) or not managed(value.get('model')):
                return False
            assistants += 1
    return assistants > 0


def native_records(agent, env, workspace):
    """Independent bounded metadata lookup, not a substitute for product handles."""
    records = {}
    if agent == 'opencode':
        db = Path(env['OPENCODE_DB']) if env.get('OPENCODE_DB') else Path(env['XDG_DATA_HOME']) / 'opencode/opencode.db'
        if not db.is_file():
            return records
        with sqlite3.connect(db.as_uri() + '?mode=ro', uri=True, timeout=1) as connection:
            connection.execute('BEGIN')
            fields = {row[1] for row in connection.execute('PRAGMA table_info(session_v2)')}
            version = 'version' if 'version' in fields else 'NULL'
            query = 'SELECT id,directory,model,' + version + ' FROM session_v2 WHERE parent_id IS NULL AND fork_session_id IS NULL AND directory=? LIMIT 201'
            for index, row in enumerate(connection.execute(query, (str(workspace),))):
                if index >= 200:
                    raise ValueError('native_record_limit')
                if opencode_managed(connection, row[0], row[2], row[3]):
                    records[row[0]] = {'path': str(db), 'id': row[0], 'cwd': str(workspace)}
        return records
    if agent == 'codex':
        roots = [Path(env['CODEX_HOME']) / 'sessions']
    elif agent == 'claude-code':
        roots = [Path(env['CLAUDE_CONFIG_DIR']) / 'projects']
    else:
        home = Path(env.get('GEMINI_CLI_HOME', env['HOME'])) / '.gemini'
        registry = load(home / 'projects.json').get('projects', {}) if (home / 'projects.json').is_file() else {}
        slug = registry.get(str(workspace))
        if not slug or '/' in slug or '\\' in slug:
            return records
        roots = [home / 'tmp' / slug / 'chats']
    seen = 0
    total = 0
    for root in roots:
        if not root.is_dir():
            continue
        pending = [(root, 0)]
        while pending:
            directory, depth = pending.pop()
            for path in directory.iterdir():
                seen += 1
                if seen > 4096:
                    raise ValueError('native_entry_limit')
                if path.is_symlink():
                    continue
                if path.is_dir() and agent != 'gemini' and depth < 8:
                    pending.append((path, depth + 1))
                    continue
                if not path.is_file() or path.suffix not in ('.json', '.jsonl'):
                    continue
                total += path.stat().st_size
                if total > 32 * 1024 * 1024:
                    raise ValueError('native_byte_limit')
                data = read_bytes(path)
                rows = [json.loads(data)] if path.suffix == '.json' else [json.loads(line) for line in data.splitlines() if line.strip()]
                for row in rows:
                    if agent == 'codex' and row.get('type') == 'session_meta':
                        meta = row['payload']
                        if meta.get('model_provider') == 'aubridge' and meta.get('source') in ('cli', 'exec') and not meta.get('parent_thread_id') and not meta.get('forked_from_id') and Path(meta.get('cwd', '/')).resolve() == workspace:
                            records[meta['id']] = {'path': str(path), 'id': meta['id'], 'cwd': str(workspace)}
                    elif agent == 'claude-code' and row.get('sessionId') and row.get('cwd') and row.get('isSidechain') is False and Path(row['cwd']).resolve() == workspace:
                        if path.stem == row['sessionId']:
                            records[row['sessionId']] = {'path': str(path), 'id': row['sessionId'], 'cwd': str(workspace)}
                    elif agent == 'gemini' and row.get('kind') == 'main' and row.get('projectHash') == digest(str(workspace).encode()) and row.get('sessionId'):
                        records[row['sessionId']] = {'path': str(path), 'id': row['sessionId'], 'cwd': str(workspace)}
    return records


def native_snapshot(agent, record, expected_fact=None):
    data_hash = hashlib.sha256()
    fact_seen = False
    if agent != 'opencode':
        data = read_bytes(record['path'])
        data_hash.update(data)
        fact_seen = expected_fact is not None and expected_fact in data
    else:
        with sqlite3.connect(Path(record['path']).as_uri() + '?mode=ro', uri=True, timeout=1) as connection:
            connection.execute('BEGIN')
            fields = {row[1] for row in connection.execute('PRAGMA table_info(session_v2)')}
            version = 'version' if 'version' in fields else 'NULL'
            agent = 'agent' if 'agent' in fields else 'NULL'
            agent_length = 'length(CAST(' + agent + ' AS BLOB))'
            header = connection.execute('SELECT id,directory,parent_id,fork_session_id,model,time_updated,' + version + ',CASE WHEN ' + agent_length + '<=?2 THEN ' + agent + ' END,' + agent_length + ' FROM session_v2 WHERE id=?1', (record['id'], LIMIT)).fetchone()
            if header and header[-1] is not None and header[-1] > LIMIT:
                raise ValueError('native_message_limit')
            if not header or header[2] is not None or header[3] is not None or Path(header[1]).resolve() != Path(record['cwd']) or not opencode_managed(connection, record['id'], header[4], header[6]):
                raise ValueError('native_record_changed')
            data_hash.update(json.dumps(header, separators=(',', ':')).encode())
            for _, kind, key, parent, ordinal, row_type, data, _ in opencode_rows(connection, record['id']):
                # Length-delimited JSON keys and data hash avoid ambiguous row concatenation.
                data_hash.update(json.dumps([kind, key, parent, ordinal, row_type, digest(data)], separators=(',', ':')).encode())
                fact_seen |= expected_fact is not None and expected_fact in data
    return data_hash.hexdigest(), fact_seen


def requests(admin, agent, native_id):
    query = urllib.parse.urlencode({'agent': agent, 'session_id': native_id})
    rows = admin('gateway?' + query).get('requests', [])
    return [row for row in rows if row.get('agent') == agent and row.get('session_id') == native_id]


def gemini_header(native_id):
    if str(uuid.UUID(native_id)) != native_id:
        raise ValueError('gemini_custom_headers_conflict')
    return 'x-session-id: ' + native_id


def opencode_seed_args(client, prompt):
    return [client, 'run', '--standalone', '--format', 'json', prompt]


def gemini_header_guard(native_id):
    expected = shlex.quote(gemini_header(native_id))
    return ('set +x\nif [ -n "${GEMINI_CLI_CUSTOM_HEADERS-}" ] && [ "${GEMINI_CLI_CUSTOM_HEADERS-}" != ' + expected + ' ]; then\n'
            "  printf '%s\\n' 'gemini_custom_headers_conflict: 自定义请求头需核对' >&2\n  exit 78\nfi\n")


def gemini_seed_headers(manifest, root, workspace, env, native_id):
    """Controlled private fixture only; not a replacement for the product loader."""
    expected = gemini_header(native_id)
    def allowed(value):
        if value not in (None, '', expected):
            raise ValueError('gemini_custom_headers_conflict')
    allowed(os.environ.get('GEMINI_CLI_CUSTOM_HEADERS'))
    for name in ('GEMINI_CLI_SYSTEM_SETTINGS_PATH', 'GEMINI_CLI_SYSTEM_DEFAULTS_PATH'):
        if os.environ.get(name):
            raise ValueError('gemini_custom_headers_conflict')
    system = Path('/Library/Application Support/GeminiCli' if os.sys.platform == 'darwin' else '/etc/gemini-cli')
    if any((system / name).exists() for name in ('settings.json', 'system-defaults.json')):
        raise ValueError('gemini_custom_headers_conflict')
    check = load(inside(root, manifest['runtime']['project_check_file']))
    context = check.get('env') or {}
    if check.get('status') != 'matched' or context.get('trust') != 'trusted' or check.get('project_alias') != workspace.name:
        raise ValueError('gemini_custom_headers_conflict')
    home = inside(root, env.get('GEMINI_CLI_HOME', env['HOME']))
    hits = {'home_gemini': home / '.gemini/.env', 'project_gemini': workspace / '.gemini/.env', 'project_plain': workspace / '.env'}
    selected = hits.get(context.get('layer'))
    if selected is None or selected.is_symlink():
        raise ValueError('gemini_custom_headers_conflict')
    selected = inside(root, selected)
    for settings_file in (home / '.gemini/settings.json', workspace / '.gemini/settings.json'):
        if not settings_file.exists():
            continue
        value = load(inside(root, settings_file))
        # Only these managed private-case settings are supported, not arbitrary SDK configs.
        model = value.get('model', {}).get('name')
        configs = value.get('modelConfigs') or {}
        if configs and (set(configs) != {'customAliases'} or set(configs['customAliases']) != {model} or configs['customAliases'][model] != {'modelConfig': {'model': model}}):
            raise ValueError('gemini_custom_headers_conflict')
        for owner in (value, value.get('security', {}).get('auth', {}), value.get('httpOptions', {})):
            for key in ('headers', 'customHeaders'):
                candidate = owner.get(key)
                if candidate not in (None, '', {}, []):
                    allowed(candidate)
            if owner.get('baseUrl'):
                raise ValueError('gemini_custom_headers_conflict')
    seen = False
    for line in read_bytes(selected).decode('utf-8-sig').splitlines():
        line = line.lstrip()
        if not line or line.startswith('#'):
            continue
        line = line.removeprefix('export ').removeprefix('export\t').lstrip()
        key, separator, raw = line.partition('=')
        if not separator:
            raise ValueError('gemini_custom_headers_conflict')
        if key.strip() != 'GEMINI_CLI_CUSTOM_HEADERS':
            continue
        if seen:
            raise ValueError('gemini_custom_headers_conflict')
        seen = True
        raw = raw.strip()
        if raw[:1] in ('\'', '"', '`'):
            quote = raw[0]; end = raw.find(quote, 1)
            if end < 0:
                raise ValueError('gemini_custom_headers_conflict')
            tail = raw[end + 1:].lstrip()
            if tail and not tail.startswith('#'):
                raise ValueError('gemini_custom_headers_conflict')
            raw = raw[1:end]
            if '\\' in raw:
                raise ValueError('gemini_custom_headers_conflict')
        else:
            if any(char in raw for char in ('\'', '"', '`')):
                raise ValueError('gemini_custom_headers_conflict')
            raw = raw.split('#', 1)[0].rstrip()
        allowed(raw)
    return expected


def prepare(args):
    manifest, root, workspace, env, admin, build, control = context(args.manifest, args.agent)
    state_file = control / 'state.json'
    if state_file.exists():
        raise ValueError('case_already_exists_use_new_private_workspace')
    if not Path(args.client).is_absolute():
        raise ValueError('official_client_requires_absolute_path')
    client = str(Path(args.client).resolve(strict=True))
    if not os.access(client, os.X_OK):
        raise ValueError('official_client_not_executable')
    fact, challenge = secrets.token_hex(24), secrets.token_hex(24)
    for filename in ('native-resume-seed.txt', 'native-resume-challenge.txt', 'native-resume-result.json'):
        if (workspace / filename).exists():
            raise ValueError('challenge_filename_already_exists')
    existing = list(native_records(args.agent, env, workspace))
    native_id = str(uuid.uuid4())
    seed_env = dict(env)
    if args.agent == 'codex':
        seed_env.update(NO_PROXY='127.0.0.1,localhost,::1', no_proxy='127.0.0.1,localhost,::1')
    if args.agent == 'gemini':
        seed_env['GEMINI_CLI_CUSTOM_HEADERS'] = gemini_seed_headers(manifest, root, workspace, env, native_id)
    write(workspace / 'native-resume-seed.txt', fact)
    write(workspace / 'native-resume-challenge.txt', challenge)
    write(control / 'continuation-prompt.txt', CONTINUE_PROMPT)
    if args.agent == 'codex':
        argv = [client, '--profile', 'aubridge', 'exec', '--skip-git-repo-check', '--color', 'never', '--json', SEED_PROMPT]
    elif args.agent == 'claude-code':
        argv = [client, '-p', '--session-id', native_id, '--output-format', 'stream-json', '--verbose', '--permission-mode', 'dontAsk', '--tools', 'Read', '--allowedTools', 'Read(./native-resume-seed.txt)', '--disable-slash-commands', SEED_PROMPT]
    elif args.agent == 'gemini':
        argv = [client, '--session-id', native_id, '--prompt', SEED_PROMPT, '--output-format', 'stream-json', '--approval-mode', 'default']
    else:
        argv = opencode_seed_args(client, SEED_PROMPT)
    command = ('cd ' + shlex.quote(str(workspace)) + ' && env -i PATH=' + shlex.quote(os.environ.get('PATH', '/usr/bin:/bin')) + ' ' +
               ' '.join(shlex.quote(key + '=' + value) for key, value in sorted(seed_env.items())) + ' ' +
               ' '.join(shlex.quote(part) for part in argv) + ' >/dev/null\n')
    if args.agent == 'gemini':
        command = gemini_header_guard(native_id) + command
    write(control / 'seed-command.txt', command)
    state = {'agent': args.agent, 'fact_sha256': digest(fact.encode()), 'challenge_sha256': digest(challenge.encode()),
             'prompt_sha256': digest(CONTINUE_PROMPT.encode()), 'existing_ids': existing,
             'expected_seed_id': native_id if args.agent in ('gemini', 'claude-code') else None,
             'setup_manifest_sha256': digest(read_bytes(args.manifest)), 'build': build, 'stage': 'prepared',
             'seed_session_source': 'caller_explicit_seed' if args.agent == 'gemini' else 'native_cli',
             'resume_session_source': 'product_launcher_explicit_resume' if args.agent == 'gemini' else 'native_cli'}
    write(state_file, state)
    print(json.dumps({'stage': 'prepared', 'agent': args.agent, 'next': 'Luna runs private seed-command.txt; then seeded. Do not use ephemeral/no-session-persistence.'}))


def seeded(args):
    manifest, root, workspace, env, admin, build, control = context(args.manifest, args.agent)
    state = load(control / 'state.json')
    if state['stage'] != 'prepared':
        raise ValueError('seed_stage_mismatch')
    records = native_records(args.agent, env, workspace)
    added = set(records) - set(state['existing_ids'])
    if len(added) != 1:
        raise ValueError('seed_requires_one_new_native_root_id')
    native_id = added.pop()
    if state['expected_seed_id'] and state['expected_seed_id'] != native_id:
        raise ValueError('seed_native_id_mismatch')
    fact = read_bytes(workspace / 'native-resume-seed.txt')
    snapshot, fact_seen = native_snapshot(args.agent, records[native_id], fact)
    if digest(fact) != state['fact_sha256'] or not fact_seen:
        raise ValueError('original_native_conversation_has_not_observed_fact')
    sessions = admin('native-sessions').get('sessions', [])
    candidates = [row for row in sessions if row.get('agent') == args.agent and row.get('project_alias') == workspace.name and row.get('state') == 'ready']
    if len(candidates) != 1:
        raise ValueError('production_native_candidate_not_unique_or_not_ready')
    baseline = requests(admin, args.agent, native_id)
    if not any(row.get('outcome') == 'ok' and row.get('provider') and row.get('served_model') for row in baseline):
        raise ValueError('seed_native_id_has_no_exact_gateway_requests')
    (workspace / 'native-resume-seed.txt').unlink()
    state.update(stage='seeded', native_id=native_id, original_fact_seen=True, native_snapshot=snapshot,
                 baseline_request_ids=[row['request_id'] for row in baseline if row.get('request_id')])
    write(control / 'state.json', state)
    write(control / 'observation-template.json', {'native_id': native_id, 'prompt_sha256': state['prompt_sha256'],
          'production_resume_clicked': False, 'terminal_opened_by_product': False, 'original_context_observed': False,
          'new_file_tool_observed': False, 'no_history_files_read': False,
          'continuation_prompt_matches_file': False})
    print(json.dumps({'stage': 'seeded', 'agent': args.agent, 'native_id': native_id,
                      'original_fact_seen': True, 'seed_file_deleted': True,
                      'next': 'CUA refresh production Sessions, click this project Continue, enter exact continuation prompt; observe tools and fill observation template.'}))


def verify(args):
    manifest, root, workspace, env, admin, build, control = context(args.manifest, args.agent)
    state = load(control / 'state.json')
    if state['stage'] != 'seeded' or digest(read_bytes(args.manifest)) != state['setup_manifest_sha256']:
        raise ValueError('seed_stage_or_private_setup_changed')
    observation = load(inside(root, args.observation))
    required = ('production_resume_clicked', 'terminal_opened_by_product', 'original_context_observed',
                'new_file_tool_observed', 'no_history_files_read', 'continuation_prompt_matches_file')
    observed = all(observation.get(key) is True for key in required)
    observed &= observation.get('native_id') == state['native_id'] and observation.get('prompt_sha256') == state['prompt_sha256']
    result = load(workspace / 'native-resume-result.json')
    fact_ok = isinstance(result.get('remembered_fact'), str) and digest(result['remembered_fact'].encode()) == state['fact_sha256']
    challenge_ok = isinstance(result.get('challenge'), str) and digest(result['challenge'].encode()) == state['challenge_sha256']
    record = native_records(args.agent, env, workspace).get(state['native_id'])
    changed = bool(record) and native_snapshot(args.agent, record)[0] != state['native_snapshot']
    new_requests = [row for row in requests(admin, args.agent, state['native_id']) if row.get('request_id') not in state['baseline_request_ids']]
    successful = [row for row in new_requests if row.get('outcome') == 'ok' and row.get('provider') and row.get('served_model')]
    usage = []
    decisions = []
    proved_ids = set()
    decision_fields = ('request_id', 'agent', 'session_id', 'outcome', 'provider', 'model',
                       'served_model', 'input_tokens', 'output_tokens')
    ledger = inside(root, manifest['runtime']['ledger_path'])
    with sqlite3.connect(ledger.as_uri() + '?mode=ro', uri=True, timeout=1) as connection:
        for row in successful:
            found = connection.execute('SELECT request_id,agent,session_id,outcome,provider,model,served_model,input_tokens,output_tokens FROM decisions WHERE request_id=? AND agent=? AND session_id=? LIMIT 2',
                                       (row['request_id'], args.agent, state['native_id'])).fetchall()
            if len(found) != 1 or any(key not in row for key in decision_fields):
                continue
            decision = dict(zip(decision_fields, found[0]))
            decisions.append(decision)
            if any(decision[key] != row[key] for key in decision_fields) or any(decision[key] is None for key in ('input_tokens', 'output_tokens')):
                continue
            found_usage = connection.execute('SELECT x.request_id,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting FROM usage_details x JOIN usage u ON u.rowid=x.usage_rowid JOIN decisions d ON d.request_id=x.request_id AND d.agent=u.agent WHERE x.request_id=? AND u.agent=? AND d.session_id=? LIMIT 2',
                                             (row['request_id'], args.agent, state['native_id'])).fetchall()
            usage.extend(found_usage)
            if len(found_usage) == 1 and found_usage[0] == (row['request_id'], decision['provider'], decision['served_model'], decision['input_tokens'], decision['output_tokens'], 'reported'):
                proved_ids.add(row['request_id'])
    request_ids = [row['request_id'] for row in successful]
    joined = len(successful) >= 2 and len(set(request_ids)) == len(request_ids) and set(request_ids) == proved_ids
    receipt = {'scope': 'private App production-handle native continuation; operator-observed tools',
               'agent': args.agent, 'native_id': state['native_id'], 'same_native_id': bool(record),
               'native_history_changed': changed, 'original_fact_seen': state['original_fact_seen'],
               'fact_recalled': fact_ok, 'new_challenge_matched': challenge_ok, 'cua_observed': observed,
               'request_decision_usage_joined': joined, 'request_ids': [row['request_id'] for row in successful],
               'decisions': decisions,
               'served': [{'provider': row['provider'], 'model': row['served_model']} for row in successful],
               'usage': [{'request_id': row[0], 'provider': row[1], 'model': row[2], 'input_tokens': row[3],
                          'output_tokens': row[4], 'accounting': row[5]} for row in usage],
               'fact_sha256': state['fact_sha256'], 'challenge_sha256': state['challenge_sha256'],
               'prompt_sha256': state['prompt_sha256'], 'setup_manifest_sha256': state['setup_manifest_sha256'],
               'seed_session_source': state.get('seed_session_source'), 'resume_session_source': state.get('resume_session_source'),
               'passed': bool(record) and changed and fact_ok and challenge_ok and observed and joined}
    write(control / 'receipt.json', receipt)
    if receipt['passed']:
        (workspace / 'native-resume-result.json').unlink()
        state['stage'] = 'verified'
        write(control / 'state.json', state)
    print(json.dumps(receipt))
    if not receipt['passed']:
        raise SystemExit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('prepare', 'seeded', 'verify'))
    parser.add_argument('--manifest', required=True)
    parser.add_argument('--agent', required=True, choices=AGENTS)
    parser.add_argument('--client', help='official absolute CLI path, needed by prepare only')
    parser.add_argument('--observation', help='private normalized CUA observation, needed by verify only')
    args = parser.parse_args()
    if args.action == 'prepare' and not args.client or args.action == 'verify' and not args.observation:
        parser.error('prepare needs --client; verify needs --observation')
    try:
        {'prepare': prepare, 'seeded': seeded, 'verify': verify}[args.action](args)
    except (OSError, ValueError, KeyError, sqlite3.Error) as error:
        # Exception strings can contain credential-bearing URLs or CLI content.
        reason = str(error) if isinstance(error, ValueError) and str(error).replace('_', '').isalnum() else type(error).__name__
        print(json.dumps({'passed': False, 'stage': args.action, 'agent': args.agent, 'reason': reason}))
        raise SystemExit(1) from None


if __name__ == '__main__':
    main()
