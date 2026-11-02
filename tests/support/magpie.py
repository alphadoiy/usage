"""External Magpie contracts: synthetic catalog, real Usage CLI, fake Codex, no vendor calls."""
import concurrent.futures
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import tomllib
import uuid

binary = str(Path(sys.argv[1]).resolve())
seen = []
delivered = []
models = [{"id": "group/coding", "owned_by": "group", "context_window": 200000,
           "reasoning_levels": ["low", "high"], "modalities": {"input": ["text", "image", "audio", "video", "future-modality"], "output": ["text"]}},
          {"id": "vendor/model", "owned_by": "vendor"},
          {"id": "vendor/video-only", "owned_by": "vendor", "modalities": {"input": ["video"], "output": ["text"]}}]

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        assert self.headers.get("Authorization") == "Bearer synthetic-gateway-key"
        if self.path.startswith('/api/hello'):
            value = {"name": "magpie", "version": "fixture"}
        elif self.path.startswith('/v1/models'):
            value = {"data": models}
        elif self.path.startswith('/v1/magpie/quotas'):
            value = {"data": [
                {"provider": "vendor", "name": "Shared", "balance": "12.30 credits", "windows": [{"name": "5h", "used": 25}]},
                {"name": "WorkBuddy", "readAt": "2026-11-02T00:47:31+08:00", "windows": [
                    {"name": "Credits", "amount": 1709.39999966, "limit": 8100, "unit": "credits"}
                ]}
            ]}
        elif self.path.startswith('/v1/magpie/usage?'):
            value = {"period": "today", "calls": 3, "errors": 1, "input": 120, "output": 30,
                     "cache_read": 40, "cache_write": 10, "reasoning": 5, "unpriced": 1, "cost": 0.0123}
        elif self.path.startswith('/v1/magpie/route'):
            value = {"route": {"asked": "group/coding", "model": "vendor/model", "rule": {"when": "tests"}}}
        else:
            value = {"rows": [], "total": 0}
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(value).encode())

    def do_POST(self):
        assert self.path == '/v1/responses'
        byok = self.headers.get('X-Fixture-Provider') == 'byok'
        assert self.headers.get("Authorization") == ('Bearer synthetic-byok-key' if byok else 'Bearer synthetic-gateway-key')
        if not byok:
            assert self.headers.get('x-openai-actor-authorization') == 'magpie'
        data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        seen.append((data['model'], self.headers.get('X-Magpie-Account'), self.headers['session_id']))
        delivered.append((self.headers.get('Authorization'), json.dumps(data)))
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        for event in [
            {'type': 'response.created', 'response': {'id': 'fixture-response'}},
            {'type': 'response.output_item.done', 'item': {'type': 'message', 'role': 'assistant', 'id': 'fixture-message',
                'content': [{'type': 'output_text', 'text': 'Synthetic fixture reply'}]}},
            {'type': 'response.completed', 'response': {'id': 'fixture-response', 'usage': {'input_tokens': 1, 'output_tokens': 1, 'total_tokens': 2}}}
        ]:
            self.wfile.write(('event: '+event['type']+'\ndata: '+json.dumps(event)+'\n\n').encode())

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix='usage-magpie-contract-') as temp:
        root = Path(temp)
        (root / '.git').mkdir()
        (root / 'config.toml').write_text('providers = ["codex"]\n[paths]\n' + '\n'.join(f'{name} = {json.dumps(str(root / name))}' for name in ['codex', 'claude', 'grok']))
        env = dict(os.environ, USAGE_CODEX_DIR=str(root / 'manager'), USAGE_CODEX_BASE_HOME=str(root / 'native'),
                   USAGE_CONFIG=str(root / 'config.toml'), USAGE_LOG_DIR=str(root / 'logs'),
                   XDG_DATA_HOME=str(root / 'data'), USAGE_LOG='off', USAGE_GATEWAY='off',
                   MAGPIE_FIXTURE_KEY='synthetic-gateway-key', CODEX_HOME=str(root / 'native'), XDG_CONFIG_HOME=str(root / 'config'), NO_PROXY='*', no_proxy='*')
        address = f'http://127.0.0.1:{server.server_port}'
        prefix = ['magpie', '--address', address, '--key-env', 'MAGPIE_FIXTURE_KEY']

        def run(args, success=True):
            result = subprocess.run([binary, *args], env=env, cwd=root, capture_output=True, text=True, timeout=30)
            assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
            return result

        listed = json.loads(run([*prefix, 'models']).stdout)
        assert listed[0]['id'] == 'group/coding'
        group = json.loads(run([*prefix, 'configure', '--model', 'group/coding']).stdout)
        duplicate = run([*prefix, 'bind', '--name', 'Another name', '--model', 'group/coding'], success=False)
        assert '--replace' in duplicate.stderr
        fixed = json.loads(run([*prefix, 'configure', '--model', 'vendor/model', '--account', 'account-a']).stdout)
        assert fixed['id'] == group['id'] and fixed['revision'] == group['revision'] + 1
        for selector in (group['id'], 'builtin:magpie'):
            assert '不能删除' in run(['codex', 'remove', selector], success=False).stderr
            assert '不能复制' in run(['codex', 'copy', selector, 'Copy'], success=False).stderr
        fake = root / 'fake-codex'
        fake.write_text('''#!/usr/bin/env python3
import json,os,sys,tomllib,urllib.request
from pathlib import Path
if sys.argv[1:]==['--help']:
    print('--no-daemon');sys.exit(0)
config=tomllib.loads((Path(os.environ['CODEX_HOME'])/'config.toml').read_text())
def merge(target, source):
    for key,value in source.items():
        if isinstance(value,dict) and isinstance(target.get(key),dict):merge(target[key],value)
        else:target[key]=value
for index,arg in enumerate(sys.argv):
    if arg=='-c':merge(config,tomllib.loads(sys.argv[index+1]))
provider=config['model_providers'][config['model_provider']]
assert config['model_provider']=='custom'
assert provider['base_url']==os.environ['EXPECTED_MAGPIE']+'/v1', provider
catalog=json.loads(Path(config['model_catalog_json']).read_text())['models']
assert catalog[0]['input_modalities']==['text','image','audio'],catalog[0]
assert [m['slug'] for m in catalog]==['group/coding','vendor/model']
assert catalog[0]['context_window']==200000
assert os.environ['USAGE_CODEX_API_KEY']=='synthetic-gateway-key'
headers=dict(provider['http_headers'],Authorization='Bearer '+os.environ['USAGE_CODEX_API_KEY'],session_id=os.environ['FIXTURE_SESSION'])
body=json.dumps({'model':config['model'],'input':'synthetic test task','stream':True}).encode()
request=urllib.request.Request(provider['base_url']+'/responses',data=body,headers=headers)
with urllib.request.urlopen(request,timeout=5) as response:
    assert b'response.completed' in response.read()
''')
        fake.chmod(0o700)
        env['EXPECTED_MAGPIE'] = address

        def launch(profile, resume=False, extra=()):
            session = str(uuid.uuid4())
            local = dict(env, FIXTURE_SESSION=session)
            args = ['codex', 'run', profile['id'], '--revision', str(profile['revision']), '--cwd', str(root), '--binary', str(fake)]
            if resume:
                args += ['--resume', session]
            args += ['--', *extra]
            result = subprocess.run([binary, *args], env=local, cwd=root, capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, (result.stdout, result.stderr)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            list(pool.map(launch, [group, dict(fixed, id='builtin:magpie')]))
        launch(group, resume=True)
        assert sorted((m, a or '') for m, a, _ in seen) == [('group/coding', ''), ('group/coding', ''), ('vendor/model', 'account-a')], seen
        assert len({session for _, _, session in seen}) == 3
        assert not list((root / 'manager').glob('gateway/**/state.json')), 'Magpie launched a Usage gateway'
        assert json.loads(run([*prefix, 'quotas']).stdout)['data'][0]['windows'][0]['used'] == 25
        assert json.loads(run([*prefix, 'summary']).stdout)['calls'] == 3
        assert json.loads(run([*prefix, 'requests']).stdout)['total'] == 0
        assert json.loads(run([*prefix, 'route', str(uuid.uuid4())]).stdout)['route']['model'] == 'vendor/model'
        runs = json.loads(run(['codex', 'runs', '--json']).stdout)
        assert len(runs) == 3
        assert all(r['finished_at'] for r in runs)
        # Onboarding and observation are exercised in a real terminal with isolated state.
        from ui_workflows import Terminal, plain
        env['USAGE_MAGPIE_URL'] = address
        terminal = Terminal(binary, [], env, root)
        try:
            terminal.until("Magpie")
            terminal.until("Magpie·内置")
            terminal.until("12.30credits")
            terminal.until("请求3次")
            terminal.until("6,390.6credits")
            terminal.until("150Token")
            displayed = plain(terminal.data)
            assert any(dot in displayed for dot in "━─"), displayed
            for removed in ("共享额度与余额", "读取于", "范围：", "估算费用", "缓存读取", "1709.39999966", "6390.60000034"):
                assert removed not in displayed, displayed
            terminal.send(b"M")
            terminal.until("返回账号")
            terminal.send(b"\r")
            terminal.send(b"j")
            terminal.until("Credits")
            terminal.send(b"v")
            terminal.until("Magpie请求与路由")
            terminal.until("尚无请求记录")
            terminal.send(b"\x1b")
            terminal.send(b"\x1b[15~")
            terminal.until("接入方式")
            terminal.until("ChatGPTOAuth")
            terminal.send(b"\x1b")
            terminal.send(b"\x1bOS")
            terminal.until("[F4Provider]")
            terminal.send(b"d")
            terminal.until("不能删除")
            terminal.send(b"c")
            # Ratatui reuses the unchanged "不能" prefix of the previous notice.
            terminal.until("复制或添加")
            terminal.send(b"\x1b")
            terminal.until("Magpie·内置")
            terminal.send(b"e")
            terminal.until("连接Magpie")
            terminal.send(b"\r")
            terminal.until("选择路由方案或模型")
            terminal.send(b"group/")
            terminal.send(b"\r")
            terminal.until("保存内置Magpie配置")
            terminal.send(b"high")
            terminal.send(b"\r")
            terminal.until("已保存内置Magpie配置")
        finally:
            terminal.close()
        # A model with no compatible input must fail before generating a catalog
        # or allowing the Agent to send a request. Other models remain usable.
        run([*prefix, 'configure', '--model', 'vendor/video-only'])
        rejected = run(['codex', 'run', group['id'], '--cwd', str(root), '--binary', str(fake)], success=False)
        assert '没有 Codex 支持的输入类型' in rejected.stderr, rejected.stderr
        run([*prefix, 'configure', '--model', 'group/coding'])
        profiles = json.loads(run(['codex', 'list', '--json']).stdout)
        assert len(profiles) == 1
        assert all(p['model_provider'] == 'magpie' for p in profiles)
        if len(sys.argv) > 2:
            # Exercise actual Codex schema and app-server configuration without a model turn.
            from native_codex_context import Client
            native = Client([binary, 'codex', 'run', group['id'], '--cwd', str(root),
                             '--binary', sys.argv[2], '--', 'app-server', '--listen', 'stdio://'],
                            env, root, root / 'native-magpie.stderr')
            try:
                config = native.rpc('config/read', {'cwd': str(root)})['config']
                assert config['model_provider'] == 'custom', config
                assert config['model'] == 'group/coding', config
                listed = native.rpc('model/list', {})['data']
                assert {m['id'] for m in listed} >= {'group/coding', 'vendor/model'}, listed
                started = native.rpc('thread/start', {'cwd': str(root), 'model': 'group/coding'})
                assert started['modelProvider'] == 'custom', started
                thread_id = started['thread']['id']
                native.complete_turn(thread_id, 'Magpie shared history fixture')
                native.rpc('thread/name/set', {'threadId': thread_id, 'name': 'Shared Magpie fixture'})
                print('Native Codex accepted Magpie configuration and generated model catalog')
            finally:
                native.close()
            # The same native thread is discoverable and resumable through an ordinary BYOK,
            # with its own key and no Magpie-specific authorization header.
            byok_config = root / 'native-byok.toml'
            byok_config.write_text(f'''model = "vendor/model"
model_provider = "another-vendor"
[model_providers.another-vendor]
name = "BYOK fixture"
base_url = "{address}/v1"
wire_api = "responses"
[model_providers.another-vendor.http_headers]
X-Fixture-Provider = "byok"
''')
            byok_env = dict(env, BYOK_NATIVE_KEY='synthetic-byok-key')
            result = subprocess.run([binary, 'codex', 'add', 'Native BYOK', '--config', str(byok_config), '--key-env', 'BYOK_NATIVE_KEY'],
                                    env=byok_env, cwd=root, text=True, capture_output=True, timeout=20)
            assert result.returncode == 0, result.stderr
            byok = Client([binary, 'codex', 'run', 'Native BYOK', '--cwd', str(root),
                           '--binary', sys.argv[2], '--', 'app-server', '--listen', 'stdio://'],
                          env, root, root / 'native-byok.stderr')
            try:
                visible = byok.rpc('thread/list', {'cwd': str(root), 'modelProviders': ['custom'], 'limit': 25})['data']
                assert thread_id in {thread['id'] for thread in visible}, visible
                resumed = byok.rpc('thread/resume', {'threadId': thread_id, 'modelProvider': 'custom', 'model': 'vendor/model'})
                assert resumed['thread']['id'] == thread_id and resumed['modelProvider'] == 'custom', resumed
                byok.complete_turn(thread_id, 'Continue through BYOK fixture')
                assert any(key == 'Bearer synthetic-byok-key' and 'Magpie shared history fixture' in body
                           and 'Continue through BYOK fixture' in body for key, body in delivered), delivered
                print('Native Magpie -> BYOK: same thread resumed with BYOK key and full shared history')
            finally:
                byok.close()
        requests_before_rejection = len(seen)
        # A missing model must fail before the Agent can send a request.
        models[:] = [models[1]]
        result = run(['codex', 'run', group['id'], '--cwd', str(root), '--binary', str(fake)], success=False)
        assert '不再提供' in result.stderr
        assert len(seen) == requests_before_rejection
        # Migrate in place, preserving saved revisions and profile identity.
        migrated = json.loads(run([*prefix, 'bind', '--name', 'Migrated', '--model', 'vendor/model', '--replace', group['id']]).stdout)
        assert migrated['id'] == group['id'] and migrated['revision'] == profiles[0]['revision'] + 1
        # Historical revisions retain their own endpoint after a connection edit.
        run(['codex', 'edit', group['id'], '--base-url', 'https://unreachable.invalid/v1'])
        historical = run(['codex', 'run', group['id'], '--revision', '1', '--cwd', str(root), '--binary', str(fake)], success=False)
        assert '不再提供' in historical.stderr, historical.stderr
        print('Magpie contracts passed: model/route catalog, parallel direct launches, pinned account, resume, observations, missing model and migration')
finally:
    server.shutdown()
    server.server_close()
