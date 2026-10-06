#!/usr/bin/env python3
"""Record sanitized Docker API fixtures from a live engine (read-only GETs).

  record.py /run/user/1000/docker.sock [OLD=NEW ...]

Your home directory is rewritten to /home/u; each OLD=NEW pair renames any
other identifying text (project, image, container or volume names). Review the
output for anything personal before committing it.

Writes info.json, system_df.json, images_inspect.json, containers_inspect.json,
content.json (index/manifest blobs, configs reduced to rootfs) and snapshots.db
(re-encoded snapshotter metadata, via ../containerd/mkbolt.py). Env, labels,
commands and network settings are dropped so no secrets end up in fixtures.
"""
import json, os, re, subprocess, sys

here = os.path.dirname(os.path.abspath(__file__))
sock = sys.argv[1]
RENAMES = [a.split('=', 1) for a in sys.argv[2:]] + [[os.path.expanduser('~'), '/home/u']]


def get(path):
    out = subprocess.check_output(['curl', '-sf', '--unix-socket', sock, 'http://d' + path])
    return json.loads(out)


def pick(d, keys):
    return {k: d[k] for k in keys if k in d}


def save(name, obj):
    text = json.dumps(obj, indent=1, sort_keys=True)
    for old, new in RENAMES:
        text = text.replace(old, new)
    with open(os.path.join(here, name), 'w') as f:
        f.write(text + '\n')


SECRET = re.compile(r'token|secret|passw|apikey|api_key|auth', re.I)
info = get('/info')
save('info.json', pick(info, ['DockerRootDir', 'Driver', 'DriverStatus', 'SecurityOptions', 'ServerVersion',
                              'Containerd', 'OperatingSystem', 'LoggingDriver']))
df = get('/system/df')
lab_keep = ('com.docker.compose.project', 'com.docker.compose.volume', 'com.docker.volume.anonymous')
out = {
    'LayersSize': df['LayersSize'],
    'Images': [pick(i, ['Id', 'RepoTags', 'RepoDigests', 'Containers', 'Created', 'Size', 'SharedSize',
                        'Descriptor', 'ParentId']) for i in df['Images']],
    'Containers': [dict(pick(c, ['Id', 'Names', 'Image', 'ImageID', 'Created', 'State', 'Status', 'SizeRw',
                                 'SizeRootFs']),
                        Mounts=[pick(m, ['Type', 'Name', 'Source', 'Destination', 'RW']) for m in c.get('Mounts', [])])
                   for c in df['Containers']],
    'Volumes': [dict(pick(v, ['Name', 'Driver', 'Mountpoint', 'CreatedAt', 'Options', 'Scope', 'UsageData']),
                     Labels={k: v for k, v in (v.get('Labels') or {}).items() if k in lab_keep})
                for v in df['Volumes']],
    'BuildCache': [dict(pick(b, ['ID', 'Type', 'InUse', 'Shared', 'Size', 'CreatedAt', 'LastUsedAt', 'UsageCount']),
                        Description='[redacted]' if SECRET.search(b.get('Description', '')) else b.get('Description', ''))
                   for b in df['BuildCache']],
}
save('system_df.json', out)
imgs = {}
for i in df['Images']:
    v = get('/images/%s/json' % i['Id'])
    imgs[i['Id']] = pick(v, ['Id', 'RepoTags', 'RootFS', 'Descriptor', 'Size', 'GraphDriver'])
save('images_inspect.json', imgs)
ctrs = {}
for c in df['Containers']:
    try:
        v = get('/containers/%s/json' % c['Id'])
    except subprocess.CalledProcessError:
        continue  # listed by /system/df but not inspectable (stale container)
    d = pick(v, ['Id', 'Name', 'LogPath', 'GraphDriver', 'Storage', 'Driver'])
    d['HostConfig'] = {'LogConfig': v.get('HostConfig', {}).get('LogConfig')}
    ctrs[c['Id']] = d
save('containers_inspect.json', ctrs)

# Content store: index/manifest blobs as-is, configs reduced to rootfs, layers as "".
root = info['DockerRootDir'] + '/containerd/daemon'
blobs = root + '/io.containerd.content.v1.content/blobs/sha256/'
content = {}
stack = [(i['Descriptor']['digest'], i['Descriptor']['mediaType']) for i in df['Images'] if 'Descriptor' in i]
while stack:
    dg, mt = stack.pop()
    if dg in content:
        continue
    p = blobs + dg.split(':')[1]
    if not os.path.exists(p):
        continue
    if 'manifest' in mt or 'index' in mt:
        text = open(p).read()
        content[dg] = text
        v = json.loads(text)
        for m in v.get('manifests', []):
            stack.append((m['digest'], m.get('mediaType', '')))
        if 'config' in v:
            stack.append((v['config']['digest'], 'config'))
        for l in v.get('layers', []):
            stack.append((l['digest'], 'layer'))
    elif mt == 'config':
        v = json.load(open(p))
        content[dg] = json.dumps(pick(v, ['architecture', 'os', 'rootfs']))
    else:
        content[dg] = ''
save('content.json', content)

# Snapshotter metadata, re-encoded without labels.
sys.path.insert(0, os.path.join(here, '..', 'containerd'))
import mkbolt  # noqa: E402
db = open(root + '/io.containerd.snapshotter.v1.overlayfs/metadata.db', 'rb').read()
with open(os.path.join(here, 'snapshots.db'), 'wb') as f:
    f.write(mkbolt.snapshot_db(mkbolt.read_snapshots(db)))
