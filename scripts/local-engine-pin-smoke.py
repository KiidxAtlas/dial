"""Apple Silicon: exercise Desktop's RPC sequence with installed binaries and an existing GGUF.

Usage: python3 scripts/local-engine-pin-smoke.py /path/to/dial /path/to/chat.gguf /path/to/atomic-server /path/to/stock-server
No downloads, persistent settings, external server or normal model cache are used.
"""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import urllib.request


def check(dial, weights, atomic, stock, root):
    project = root / 'project'
    project.mkdir()
    models = []
    for name in ('first', 'second'):
        snapshot = root / 'hub' / ('models--fixture--' + name) / 'snapshots/revision'
        snapshot.mkdir(parents=True)
        try:
            os.link(weights, snapshot / 'model.gguf')
        except OSError:
            shutil.copyfile(weights, snapshot / 'model.gguf')
        models.append(str(snapshot))
    env = dict(os.environ, HOME=str(root), DIAL_HOME=str(root / 'state'), HF_HUB_CACHE=str(root / 'hub'))
    for name, binary, var in [('atomic', atomic, 'DIAL_ATOMIC_SERVER'), ('stock', stock, 'DIAL_LLAMA_SERVER')]:
        wrapper = root / name
        wrapper.write_text('#!/bin/sh\nif [ "$1" = -m ]; then printf "%s\\n" "$@" > ' + shlex.quote(str(root / (name + '.args'))) + '; fi\nexec ' + shlex.quote(binary) + ' "$@"\n')
        wrapper.chmod(0o700)
        env[var] = str(wrapper)
    for restart in (False, True):
        with subprocess.Popen([dial, '-C', project, 'rpc'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as proc:
            try:
                ready = json.loads(proc.stdout.readline())
                def request(method, **params):
                    params['session_id'] = ready['session_id']
                    proc.stdin.write(json.dumps(dict(id='smoke', method=method, params=params)) + '\n')
                    proc.stdin.flush()
                    for line in proc.stdout:
                        msg = json.loads(line)
                        if msg.get('id') == 'smoke':
                            assert 'error' not in msg, (method, msg.get('error'))
                            return msg['result']
                    raise RuntimeError('RPC closed before answering ' + method)
                def pins():
                    rows = request('list_models', provider='mlx')['models']
                    return {r['model_id']: r['local_engine'] for r in rows if r.get('local_engine')}
                listed = pins()
                if restart:
                    assert listed[models[0]]['selected'] == 'llama-cpp'
                    assert listed[models[1]]['selected'] == 'auto'
                    break
                assert all(listed[m]['selected'] == 'auto' for m in models)
                for engine, name in [('atomic', 'atomic'), ('llama-cpp', 'stock')]:
                    request('set_setting', key=listed[models[0]]['setting'], scope='global', value=engine)
                    assert pins()[models[0]]['selected'] == engine
                    assert pins()[models[1]]['selected'] == 'auto'
                    assert request('set_model', provider='mlx', model_id=models[0], prepare_local_model=True)['local_ready']
                    args = (root / (name + '.args')).read_text().splitlines()
                    assert args[args.index('--host') + 1] == '127.0.0.1'
                    port = args[args.index('--port') + 1]
                    query = dict(model='default_model', messages=[dict(role='user', content='Say hello.')], max_tokens=12)
                    req = urllib.request.Request(f'http://127.0.0.1:{port}/v1/chat/completions', json.dumps(query).encode(), {'Content-Type':'application/json'})
                    with urllib.request.urlopen(req, timeout=30) as reply:
                        assert json.load(reply)['choices'][0]['message']['content'].strip()
                    print(name + ': Dial-owned load and chat passed', flush=True)
            finally:
                proc.stdin.close()
                try:
                    proc.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    raise
    print('Per-model isolation and restart persistence passed')


if __name__ == '__main__':
    with tempfile.TemporaryDirectory(prefix='dial-engine-pin-') as temp:
        check(*(str(Path(arg).resolve(strict=True)) for arg in sys.argv[1:]), Path(temp))
