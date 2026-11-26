"""Exercise Desktop's cold selection and first prompt against a real dial rpc process."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def smoke(binary, weights, server, root, missing):
    project = root / 'project'
    project.mkdir()
    snapshot = root / 'hub/models--fixture--stories/snapshots/revision'
    snapshot.mkdir(parents=True)
    shutil.copyfile(weights, snapshot / 'stories.gguf')
    env = dict(os.environ, DIAL_HOME=str(root / 'state'), HOME=str(root),
               USERPROFILE=str(root), HF_HUB_CACHE=str(root / 'hub'),
               DIAL_LLAMA_SERVER=str(root / 'missing-llama-server.exe') if missing else server)
    with subprocess.Popen([binary, '-C', project, 'rpc'], env=env, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, text=True, encoding='utf-8') as proc:
        try:
            ready = json.loads(proc.stdout.readline())
            assert ready['features']['prepare_local_model']
            phases = set()

            def request(i, method, **params):
                params['session_id'] = ready['session_id']
                proc.stdin.write(json.dumps(dict(id=i, method=method, params=params)) + '\n')
                proc.stdin.flush()
                for line in proc.stdout:
                    msg = json.loads(line)
                    if msg.get('event') == 'model_progress' and msg.get('request_id') == i:
                        phases.add(msg['progress']['phase'])
                    if msg.get('event') == 'text' and msg.get('delta') and msg.get('request_id') == i:
                        phases.add('streaming text')
                    if msg.get('id') == i:
                        return msg
                raise RuntimeError('RPC closed without answering ' + method)

            # The picker lists each cached GGUF file, not its snapshot directory.
            result = request(1, 'set_model', provider='mlx', model_id=str(snapshot / 'stories.gguf'), prepare_local_model=True)
            if missing:
                assert 'missing-llama-server.exe' in result['error']['message'], result
                return
            assert result.get('result', {}).get('local_ready'), result
            result = request(2, 'send', text='Tell a short story.')
            assert result.get('result', {}).get('text', '').strip(), result.get('error', 'empty answer')
            assert 'processing' in phases and phases.intersection({'generating', 'streaming text'}), phases
            print('Prompt produced text; reported phases:', sorted(phases))
        finally:
            proc.stdin.close()
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                raise


if __name__ == '__main__':
    for fail in (True, False):
        with tempfile.TemporaryDirectory(prefix='dial model smoke ') as directory:
            smoke(str(Path(sys.argv[1]).resolve()), os.environ['DIAL_TEST_GGUF'],
                  os.environ['DIAL_LLAMA_SERVER'], Path(directory), fail)
