#!/usr/bin/env python3
"""Exercise the real Caddy proxy with a disposable local CA, without public DNS."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='graphicle:0.4.0')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    name = 'graphicle-tls-' + uuid.uuid4().hex[:10]
    proxy = name + '-proxy'
    domain = 'graphicle-qualification.example.test'
    record = {'public_deployment': False, 'tls_certificate': 'disposable Caddy internal CA', 'passed': False}

    def docker(*values):
        return subprocess.check_output(['docker', *map(str, values)], text=True, stderr=subprocess.PIPE, timeout=60).strip()

    try:
        with tempfile.TemporaryDirectory(prefix='graphicle-caddy-') as temp:
            directory = Path(temp)
            config = Path(__file__).resolve().parents[3]/'deploy/Caddyfile'
            (directory/'Caddyfile').write_text(config.read_text().replace('{$GRAPHICLE_DOMAIN} {', '{$GRAPHICLE_DOMAIN} {\n\ttls internal'))
            docker('run', '-d', '--name', name, '--cap-drop=ALL', '--security-opt=no-new-privileges:true',
                args.image, 'serve', '--port', '8854', '--public-origin', 'https://'+domain)
            docker('run', '-d', '--name', proxy, '--network', 'container:'+name,
                '-e', 'GRAPHICLE_DOMAIN='+domain, '-v', str(directory/'Caddyfile')+':/etc/caddy/Caddyfile:ro', 'caddy:2.10.2-alpine')
            for attempt in range(50):
                try:
                    docker('cp', proxy+':/data/caddy/pki/authorities/local/root.crt', directory/'root.crt')
                    break
                except subprocess.CalledProcessError:
                    if attempt == 49: raise
                    time.sleep(.2)
            (directory/'root.crt').chmod(0o644)
            docker('cp', directory/'root.crt', name+':/tmp/root.crt')
            curl = ['exec', name, 'curl', '-sS', '--cacert', '/tmp/root.crt', '--resolve', domain+':443:127.0.0.1']
            for attempt in range(50):
                try:
                    reply = docker(*curl, '-w', '\n%{ssl_verify_result}', 'https://'+domain+'/health')
                    body, verify = reply.rsplit('\n', 1)
                    health = json.loads(body)
                    if verify == '0' and health['status'] == 'ok': break
                except (subprocess.CalledProcessError, ValueError, KeyError):
                    if attempt == 49: raise
                    time.sleep(.2)
            assert health['hosted'] and health['public_origin'] == 'https://'+domain
            rejected = docker(*curl, '-o', '/dev/null', '-w', '%{http_code}', '-H', 'Origin: https://untrusted.example.test', 'https://'+domain+'/health')
            assert rejected == '403'
            record.update(passed=True, https_health=health, certificate_verification=verify, hostile_origin_status=rejected,
                shared_network_namespace=True, host_ports_published=False,
                image_id=json.loads(docker('image', 'inspect', args.image))[0]['Id'])
    except Exception as error:
        record['error'] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        record['image'] = args.image
        args.output.write_text(json.dumps(record, indent=2)+'\n')
        for container in (proxy, name):
            subprocess.run(['docker', 'rm', '-f', '-v', container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(json.dumps(record, indent=2))


if __name__ == '__main__':
    main()
