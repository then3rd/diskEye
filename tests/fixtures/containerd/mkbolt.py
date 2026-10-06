#!/usr/bin/env python3
"""Write small bbolt databases shaped like a containerd snapshotter metadata.db.

Used to (re)generate the binary fixtures:
  mkbolt.py k3s  > k3s-snapshots.db        # handwritten k3s layout
  mkbolt.py k3s-meta > k3s-meta.db          # containerd meta.db with images
  mkbolt.py json snaps.json > snapshots.db  # [{key,id,kind,parent,size}] list

The writer exercises inline buckets, leaf pages with overflow and a branch
page so the Rust reader's code paths are all covered.
"""
import json, struct, sys

PSZ = 4096


def fnv64a(b):
    h = 0xcbf29ce484222325
    for c in b:
        h = ((h ^ c) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def uvarint(x):
    out = bytearray()
    while x >= 0x80:
        out.append((x & 0x7F) | 0x80)
        x >>= 7
    out.append(x)
    return bytes(out)


def varint(x):
    return uvarint((x << 1) ^ (x >> 63) if x >= 0 else ((-x) << 1) - 1)


class Writer:
    def __init__(self):
        self.pages = {}  # pgid -> bytes (may span several pages)
        self.next = 3

    def alloc(self, data):
        pgid = self.next
        n = max(1, -(-len(data) // PSZ))
        self.next += n
        self.pages[pgid] = data
        return pgid, n

    @staticmethod
    def leaf(pgid, items, overflow=0):
        """items: [(key, value, is_bucket)] sorted."""
        hdr = struct.pack('<QHHI', pgid, 0x02, len(items), overflow)
        elems, data = bytearray(), bytearray()
        base = 16 + 16 * len(items)
        for i, (k, v, b) in enumerate(items):
            pos = base + len(data) - (16 + 16 * i)
            elems += struct.pack('<IIII', 1 if b else 0, pos, len(k), len(v))
            data += k + v
        return hdr + bytes(elems) + bytes(data)

    def bucket(self, items, inline=True):
        items = sorted(items, key=lambda x: x[0])
        if inline:
            return struct.pack('<QQ', 0, 0) + self.leaf(0, items)
        page = self.leaf(0, items)
        n = max(1, -(-len(page) // PSZ))
        pgid, _ = self.alloc(b'')
        self.next = pgid + n
        self.pages[pgid] = self.leaf(pgid, items, n - 1)
        return struct.pack('<QQ', pgid, 0)

    def branch_bucket(self, items, split):
        """A bucket whose root is a branch page over two leaf pages."""
        items = sorted(items, key=lambda x: x[0])
        halves = [items[:split], items[split:]]
        kids = []
        for h in halves:
            page = self.leaf(0, h)
            n = max(1, -(-len(page) // PSZ))
            pgid = self.next
            self.next += n
            self.pages[pgid] = self.leaf(pgid, h, n - 1)
            kids.append((h[0][0], pgid))
        bp = self.next
        self.next += 1
        hdr = struct.pack('<QHHI', bp, 0x01, len(kids), 0)
        elems, data = bytearray(), bytearray()
        base = 16 + 16 * len(kids)
        for i, (k, pg) in enumerate(kids):
            pos = base + len(data) - (16 + 16 * i)
            elems += struct.pack('<IIQ', pos, len(k), pg)
            data += k
        self.pages[bp] = hdr + bytes(elems) + bytes(data)
        return struct.pack('<QQ', bp, 0)

    def finish(self, root_items):
        root_page = self.leaf(0, sorted(root_items, key=lambda x: x[0]))
        rp = self.next
        self.next += 1
        self.pages[rp] = self.leaf(rp, sorted(root_items, key=lambda x: x[0]))
        assert len(root_page) <= PSZ
        out = bytearray(PSZ * self.next)
        for txid, off in ((1, 0), (2, PSZ)):
            m = struct.pack('<IIIIQQQQQ', 0xED0CDAED, 2, PSZ, 0, rp, 0, 2, self.next, txid)
            page = struct.pack('<QHHI', off // PSZ, 0x04, 0, 0) + m + struct.pack('<Q', fnv64a(m))
            out[off:off + len(page)] = page
        out[2 * PSZ:2 * PSZ + 16] = struct.pack('<QHHI', 2, 0x10, 0, 0)
        for pg, data in self.pages.items():
            out[pg * PSZ:pg * PSZ + len(data)] = data
        return bytes(out)


def snapshot_db(recs):
    w = Writer()
    items = []
    for r in recs:
        fields = [(b'id', uvarint(r['id']), False), (b'kind', bytes([r['kind']]), False)]
        if r.get('parent'):
            fields.append((b'parent', r['parent'].encode(), False))
        if r.get('size') is not None:
            fields.append((b'size', varint(r['size']), False))
        labels = [(b'containerd.io/snapshot.ref', r['key'].split('/', 2)[2].encode(), False)] if r['kind'] == 3 else []
        fields.append((b'labels', w.bucket(labels), True))
        items.append((r['key'].encode(), w.bucket(fields), True))
    snaps = w.branch_bucket(items, max(1, len(items) // 2))
    parents = w.bucket([(b'\x01\x00\x02', b'placeholder', False)], inline=False)
    v1 = w.bucket([(b'snapshots', snaps, True), (b'parents', parents, True)], inline=False)
    return w.finish([(b'v1', v1, True)])


def read_snapshots(data):
    """Parse a real snapshotter metadata.db into the record list snapshot_db takes."""
    psz = struct.unpack_from('<I', data, 24)[0]
    metas = [struct.unpack_from('<QQ', data, off + 32) + struct.unpack_from('<Q', data, off + 64)
             for off in (0, psz)]
    root = max(metas, key=lambda m: m[2])[0]

    def items(pg, inline=None):
        buf, off = (inline, 0) if pg == 0 else (data, pg * psz)
        _, flags, count, _ = struct.unpack_from('<QHHI', buf, off)
        for i in range(count):
            e = off + 16 + 16 * i
            if flags & 1:
                pos, ks, child = struct.unpack_from('<IIQ', buf, e)
                yield from items(child)
            else:
                f, pos, ks, vs = struct.unpack_from('<IIII', buf, e)
                yield buf[e + pos:e + pos + ks], f, buf[e + pos + ks:e + pos + ks + vs]

    def sub(pg, inline, key):
        for k, f, v in items(pg, inline):
            if k == key:
                r = struct.unpack_from('<Q', v)[0]
                return r, v[16:]

    def dec(b):
        x = s = 0
        for c in b:
            x |= (c & 0x7f) << s
            s += 7
            if c < 0x80:
                break
        return x

    v1 = sub(root, None, b'v1')
    snaps = sub(*v1, b'snapshots')
    recs = []
    for k, f, v in items(*snaps):
        r, il = struct.unpack_from('<Q', v)[0], v[16:]
        d = {kk: vv for kk, ff, vv in items(r, il)}
        u = dec(d[b'size']) if b'size' in d else None
        recs.append(dict(key=k.decode(), id=dec(d[b'id']), kind=d[b'kind'][0],
                         parent=d.get(b'parent', b'').decode() or None,
                         size=None if u is None else (u >> 1) ^ -(u & 1)))
    return recs


def tree_db(d):
    """Nested dict (bytes leaves) as a bbolt file; small buckets are stored inline."""
    w = Writer()

    def conv(node, top=False):
        items = []
        for k, v in node.items():
            k = k.encode() if isinstance(k, str) else k
            if isinstance(v, dict):
                items.append((k, conv(v), True))
            else:
                items.append((k, v.encode() if isinstance(v, str) else v, False))
        return items if top else w.bucket(items, inline=len(items) < 4)

    return w.finish(conv(d, top=True))


def k3s_meta():
    h = lambda c: 'sha256:' + c * (64 // len(c))
    oci_idx = 'application/vnd.oci.image.index.v1+json'
    t = lambda dg, mt: {'target': {'digest': dg, 'mediatype': mt, 'size': uvarint(1024)}, 'createdat': b'\x01'}
    imgs = {
        'registry.k8s.io/pause:3.6': t(h('b1'), oci_idx),
        'registry.k8s.io/pause@' + h('b1'): t(h('b1'), oci_idx),
        'docker.io/library/busybox:latest': t(h('d1'), 'application/vnd.oci.image.manifest.v1+json'),
    }
    return tree_db({'v1': {'k8s.io': {'images': imgs, 'content': {'blob': {}}}, 'version': uvarint(3)}})


def k3s():
    import hashlib
    d = {i: 'sha256:' + str(i) * 64 for i in range(1, 5)}
    c23 = 'sha256:' + hashlib.sha256(f'{d[2]} {d[3]}'.encode()).hexdigest()
    c14 = 'sha256:' + hashlib.sha256(f'{d[1]} {d[4]}'.encode()).hexdigest()
    ns = 'k8s.io'
    recs = [
        dict(key=f'{ns}/10/{d[1]}', id=1, kind=3, size=7340032),
        dict(key=f'{ns}/11/{d[2]}', id=2, kind=3, size=1048576),
        dict(key=f'{ns}/12/{c23}', id=3, kind=3, parent=f'{ns}/11/{d[2]}', size=13631488),
        dict(key=f'{ns}/13/{c14}', id=4, kind=3, parent=f'{ns}/10/{d[1]}', size=2097152),
        dict(key=f'{ns}/20/' + 'e1' * 32, id=5, kind=2, parent=f'{ns}/10/{d[1]}'),
        dict(key=f'{ns}/21/' + 'e2' * 32, id=6, kind=2, parent=f'{ns}/12/{c23}'),
    ]
    return snapshot_db(recs)


if __name__ == '__main__':
    if sys.argv[1] == 'k3s':
        sys.stdout.buffer.write(k3s())
    elif sys.argv[1] == 'k3s-meta':
        sys.stdout.buffer.write(k3s_meta())
    else:
        sys.stdout.buffer.write(snapshot_db(json.load(open(sys.argv[2]))))
