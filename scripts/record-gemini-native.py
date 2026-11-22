"""Capture actual Gemini CLI wire shapes against a LOCAL synthetic server.

This does not validate aubridge or a real upstream. Raw captures are private and
may include CLI system instructions. The public receipt contains shapes only.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import tempfile
import threading


def private_write(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write(value)


def shapes(value):
    if isinstance(value, dict):
        return {key: shapes(item) for key, item in value.items()}
    if isinstance(value, list):
        # Distinct complete shapes; no prompt/tool-result strings.
        unique = {}
        for item in value:
            shape = shapes(item)
            unique[json.dumps(shape, sort_keys=True)] = shape
        return list(unique.values())
    return type(value).__name__


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--client', required=True)
    parser.add_argument('--output', required=True, help='private capture directory')
    parser.add_argument('--managed-auto-preset', action='store_true', help='A/B official custom alias with no inherited generation config')
    parser.add_argument('--approval-mode', choices=['default'], default='default')
    args = parser.parse_args()
    client = str(Path(args.client).resolve())
    destination = Path(args.output).resolve()
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    captures = []
    with tempfile.TemporaryDirectory(prefix='aubridge-gemini-recorder-') as directory:
        root = Path(directory)
        home, project = root / 'home', root / 'workspace'
        (home / '.gemini').mkdir(parents=True)
        project.mkdir(mode=0o700)
        nonce = 'gemini-local-' + secrets.token_hex(16)
        (project / 'probe.txt').write_text(nonce + '\n')
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_GET(self):
                model = {'name': 'models/aubridge', 'displayName': 'Local capture only', 'supportedGenerationMethods': ['generateContent', 'streamGenerateContent'], 'inputTokenLimit': 32768, 'outputTokenLimit': 4096}
                payload = model if '/models/aubridge' in self.path else {'models': [model]}
                data = json.dumps(payload).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(data)
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                request = json.loads(body)
                captures.append({'path': self.path, 'headers': dict(self.headers), 'body': request})
                private_write(destination / f'request-{len(captures):02}.json', json.dumps(captures[-1], ensure_ascii=False, indent=2))
                callback = any('functionResponse' in part for content in request.get('contents', []) for part in content.get('parts', []))
                parts = [{'text': nonce}] if callback else [{'functionCall': {'name': 'read_file', 'args': {'file_path': str(project / 'probe.txt')}}}]
                payload = {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': parts}, 'finishReason': 'STOP'}], 'usageMetadata': {'promptTokenCount': 100, 'candidatesTokenCount': 8, 'totalTokenCount': 108}, 'modelVersion': 'aubridge', 'responseId': 'local-capture'}
                if ':countTokens' in self.path:
                    payload = {'totalTokens': 100}
                streaming = ':streamGenerateContent' in self.path
                data = ('data: ' + json.dumps(payload) + '\n\n').encode() if streaming else json.dumps(payload).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream' if streaming else 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        with http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            base = f'http://127.0.0.1:{server.server_port}'
            settings = {'security': {'auth': {'selectedType': 'gemini-api-key'}}, 'model': {'name': 'aubridge'}}
            if args.managed_auto_preset:
                settings['modelConfigs'] = {'customAliases': {'aubridge': {'modelConfig': {'model': 'aubridge'}}}}
            private_write(home / '.gemini/settings.json', json.dumps(settings))
            private_write(home / '.gemini/.env', f'GOOGLE_GEMINI_BASE_URL={base}\nGEMINI_API_KEY=aubridge-gemini\n')
            env = {key: value for key, value in os.environ.items() if not any(token in key.upper() for token in ['GEMINI', 'GOOGLE', 'VERTEX', 'GCLOUD', 'API_KEY', 'TOKEN', 'SECRET'])}
            env.update(HOME=str(home), GEMINI_CLI_HOME=str(home), XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'), XDG_STATE_HOME=str(root / 'state'), XDG_CACHE_HOME=str(root / 'cache'), TMPDIR=str(root / 'tmp'), GOOGLE_GEMINI_BASE_URL=base, GEMINI_API_KEY='aubridge-gemini', NO_COLOR='1', CI='true', GEMINI_CLI_TRUST_WORKSPACE='true')
            (root / 'tmp').mkdir()
            command = [client, '--prompt', 'Use the read_file tool to read probe.txt. Reply with its exact contents only.', '--output-format', 'stream-json', '--approval-mode', args.approval_mode]
            process = subprocess.Popen(command, cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            timed_out = False
            try:
                stdout, stderr = process.communicate(timeout=90)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
            finally:
                server.shutdown()
            private_write(destination / 'cli.stdout', stdout.decode(errors='replace'))
            private_write(destination / 'cli.stderr', stderr.decode(errors='replace'))
            receipt = {'scope': 'LOCAL SYNTHETIC wire capture only; not gateway/provider acceptance', 'exit_code': process.returncode, 'timed_out': timed_out, 'requests': [{'path': item['path'].split('?')[0], 'header_names': sorted(item['headers']), 'shape': shapes(item['body'])} for item in captures], 'managed_auto_preset': args.managed_auto_preset, 'callback_seen': any('functionResponse' in part for item in captures for content in item['body'].get('contents', []) for part in content.get('parts', [])), 'nonce_sha256': hashlib.sha256(nonce.encode()).hexdigest()}
            private_write(destination / 'receipt.json', json.dumps(receipt, indent=2))
            print(json.dumps({'scope': receipt['scope'], 'exit_code': process.returncode, 'requests': len(captures), 'callback_seen': receipt['callback_seen'], 'receipt': str(destination / 'receipt.json')}))
            if timed_out or not receipt['callback_seen']:
                raise SystemExit(1)


if __name__ == '__main__':
    main()
