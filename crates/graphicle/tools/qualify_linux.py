#!/usr/bin/env python3
"""Run the built Linux image with disposable state; keep no account credentials."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import uuid
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    version = tomllib.loads((Path(__file__).resolve().parents[1]/'Cargo.toml').read_text())['package']['version']
    parser.add_argument('--image', default='graphicle:' + version)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    name = 'graphicle-qualification-' + uuid.uuid4().hex[:10]
    volume = name + '-data'
    record = {'scope': 'local Linux container qualification; no public host or live provider tenant', 'image': args.image}

    def docker(*values, timeout=60):
        return subprocess.check_output(['docker', *map(str, values)], text=True, timeout=timeout).strip()

    def execute(*values):
        return docker('exec', name, *values)

    def cli(*values):
        return json.loads(execute('/usr/local/bin/graphicle', *values))

    def health():
        for _ in range(50):
            try:
                value = json.loads(execute('curl', '-fsS', 'http://127.0.0.1:8854/health'))
                if value['status'] == 'ok':
                    return value
            except (subprocess.CalledProcessError, json.JSONDecodeError):
                time.sleep(.2)
        raise RuntimeError('Container health did not become ready')

    try:
        image = json.loads(docker('image', 'inspect', args.image))[0]
        record.update(image_id=image['Id'], architecture=image['Architecture'], os=image['Os'], user=image['Config']['User'])
        record['version'] = docker('run', '--rm', args.image, '--version')
        # `--version` names the edition: graphicle <version> (core) or (pro).
        assert record['version'] in {f'graphicle {version} (core)', f'graphicle {version} (pro)'}
        assert record['os'] == 'linux' and record['user'] == '10001:10001'
        docker('volume', 'create', volume)
        docker('run', '-d', '--name', name, '--cap-drop=ALL', '--security-opt=no-new-privileges:true',
               '-v', f'{volume}:/var/lib/graphicle', args.image,
               'serve', '--port', '8854', '--public-origin', 'https://graphicle-qualification.example.test')
        record['health'] = health()
        assert record['health']['hosted'] and not record['health']['self_service_workspace_creation']
        record['uid'] = execute('id', '-u')
        record['state_permissions'] = execute('stat', '-c', '%a %u %g', '/var/lib/graphicle')
        assert record['uid'] == '10001' and record['state_permissions'] == '700 10001 10001'
        cli('init', '--name', 'Disposable Linux qualification')
        with tempfile.TemporaryDirectory(prefix='graphicle-linux-sources-') as temp:
            folder = Path(temp)
            (folder/'rules.md').write_text('The Copperfern exporter needs CSV and access-revocation checks.\n')
            with zipfile.ZipFile(folder/'schedule.docx', 'w') as archive:
                archive.writestr('word/document.xml', '<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>The release review is Thursday at 15:00 UTC.</w:t></w:r></w:p></w:body></w:document>')
            execute('mkdir', '/tmp/sources')
            docker('cp', str(folder) + '/.', name + ':/tmp/sources')
            synced = cli('sync', '/tmp/sources')
            record['synced_project'] = synced['repository_id']
            answer = cli('context', 'What checks does the Copperfern exporter need?')
            assert 'access-revocation' in answer['markdown']
            document = cli('context', 'When is the release review scheduled?')
            assert '15:00 UTC' in document['markdown']
        record['text_and_docx_import'] = True
        record['external_model_calls'] = 0
        record['binary_sha256'] = execute('sha256sum', '/usr/local/bin/graphicle').split()[0]
        state = json.loads(docker('inspect', name))[0]
        assert not state['HostConfig']['PortBindings']
        record['host_ports_published'] = False
        rejected = execute('curl', '-sS', '-w', '\n%{http_code}',
            '-X', 'POST', '-H', 'Content-Type: application/json', '-H', 'Host: graphicle-qualification.example.test',
            '-H', 'Origin: https://graphicle-qualification.example.test', '--data', '{"name":"Not allowed"}', 'http://127.0.0.1:8854/api/workspaces')
        body, status = rejected.rsplit('\n', 1)
        record['public_workspace_creation_status'] = status
        record['public_workspace_creation_error'] = json.loads(body).get('error')
        assert status == '400' and 'administrator-created workspaces' in record['public_workspace_creation_error'], 'Hosted workspace creation was not explicitly rejected'
        before = answer['snapshot']
        started = time.monotonic()
        docker('stop', '-t', '10', name)
        record['shutdown_seconds'] = round(time.monotonic() - started, 3)
        stopped = json.loads(docker('inspect', name))[0]
        assert stopped['State']['ExitCode'] == 0
        docker('start', name)
        health()
        answer = cli('context', 'What checks does the Copperfern exporter need?')
        assert answer['snapshot'] == before and 'access-revocation' in answer['markdown']
        record['restart_preserves_sources'] = True
        record['passed'] = True
    except Exception as error:
        record['passed'] = False
        record['error'] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(record, indent=2) + '\n')
        subprocess.run(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(['docker', 'volume', 'rm', volume], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(json.dumps(record, indent=2))


if __name__ == '__main__':
    main()
