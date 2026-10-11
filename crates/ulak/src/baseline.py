"""Read only content inventory; input paths come from sync.rs selection.

Open directories relative to descriptors so a symlinked parent cannot turn a
selected project path into access to unrelated server data. Verified parent
descriptors are reused across the sorted request, bounded so a small fd
limit cannot fail a large inventory.

A directory answers with its mode, never through the cache: the survey needs
only to know which synced directories the push must still name.

Three request shapes. A JSON list of paths answers every path. A JSON object
{"dirs": {dir: {"names": [...], "digest": hex}}, "dirmodes": {dir: mode}}
is the full survey: per directory the client sends its own fingerprints'
digest (SHA-256 over "name\0fingerprint\n" in the given order, "-" for a
missing file), and the server answers only for directories whose digest
differs, with every name of that directory, plus the directories whose mode
differs. A quiet restart therefore sends every name up (cheap under ssh
compression) and almost nothing down; baseline.rs computes the same digest.

{"verdicts": [...]} serves the deletion judgement alone (sync.rs
judge_doomed), and answers every row it names instead of failing on one:
its fingerprint (or null when absent) under "remote"; under "structural",
why the server changed what the path is (a parent that is not a real
directory, or a leaf that is neither a regular file nor a symlink); under
"held", why it cannot be read right now (a file changing while it is read,
a permission refusal, any other error). The other two shapes fail whole on
any of these, so one such row stopped every deletion in the round. Only a
problem with the request itself, such as a missing project directory,
fails it.

An optional second argument names a digest cache. It answers for a file only
while the recorded device, inode, size, mode and nanosecond times still match
and the hashing moment lay SLACK past the file's last change; hashcache.rs
states the rule and the two must not diverge. A cache that cannot be read or
written is an empty cache, never a failed comparison.

Needs Python 3.7 or later (time.time_ns); an older python3 is refused right
below, before anything that needs 3.7 runs. An interpreter compiles the whole
file before running any of it, so the file stays ASCII and parses as Python
3.4 (baseline.rs pins both): an old one must get far enough to say why.
"""
import sys

if sys.version_info < (3, 7):
    sys.exit('ulak needs Python 3.7 or later on the server; python3 there is %d.%d'
             % tuple(sys.version_info[:2]))

import collections
import errno
import hashlib
import json
import os
import stat
import time

SLACK_NS = 2000000000
MAX_PARENTS = 64


def stamp(m):
    return [m.st_dev, m.st_ino, m.st_size, m.st_mode, m.st_mtime_ns, m.st_ctime_ns]


class Parents:
    """Directory descriptors opened component by component, never through a symlink."""

    def __init__(self, root):
        self.root = root
        self.open = collections.OrderedDict()

    def get(self, parts):
        if not parts:
            return self.root
        key = '/'.join(parts)
        fd = self.open.get(key)
        if fd is not None:
            self.open.move_to_end(key)
            return fd
        parent = self.get(parts[:-1])
        fd = os.open(parts[-1], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
        self.open[key] = fd
        while len(self.open) > MAX_PARENTS:
            _, old = self.open.popitem(last=False)
            os.close(old)
        return fd

    def close(self):
        for fd in self.open.values():
            os.close(fd)


def locate(parents, path):
    parts = path.split('/')
    if any(p in ('', '.', '..') or '\x00' in p for p in parts):
        raise ValueError('unsafe relative path')
    try:
        parent = parents.get(parts[:-1])
        before = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        return None
    return parent, parts[-1], before


def digest(parent, name, before):
    try:
        if stat.S_ISLNK(before.st_mode):
            result = 'l:' + hashlib.sha256(os.readlink(os.fsencode(name), dir_fd=parent)).hexdigest()
        elif stat.S_ISREG(before.st_mode):
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
            with os.fdopen(fd, 'rb') as source:
                if stamp(os.fstat(source.fileno())) != stamp(before):
                    raise ValueError('file changed while opening')
                value = hashlib.sha256()
                for block in iter(lambda: source.read(65536), b''):
                    value.update(block)
            result = 'f:' + str(stat.S_IMODE(before.st_mode)) + ':' + value.hexdigest()
        else:
            raise ValueError('selected leaf changed type')
        if stamp(os.stat(name, dir_fd=parent, follow_symlinks=False)) != stamp(before):
            raise ValueError('file changed while hashing')
        return result
    except FileNotFoundError:
        return None


def kind(mode):
    if stat.S_ISLNK(mode):
        return 'a symlink'
    if stat.S_ISDIR(mode):
        return 'a directory'
    if stat.S_ISREG(mode):
        return 'a file'
    return 'a special file'


def verdict(parents, path):
    """One doomed row for the deletion judgement: ('remote', fingerprint or
    None), ('structural', why) or ('held', why, denied). Parents open one
    component at a time without following a symlink, as for every shape,
    so no byte is read through a parent the server replaced."""
    parts = path.split('/')
    if any(p in ('', '.', '..') or '\x00' in p for p in parts):
        return 'structural', path + ' is not a path inside the project'
    try:
        parent = parents.root
        for depth in range(1, len(parts)):
            try:
                child = parents.get(parts[:depth])
            except FileNotFoundError:
                return 'remote', None
            except OSError as error:
                # A parent that is not a directory is ENOTDIR. A symlink is
                # ENOTDIR too on Darwin 25 (symlink, dangling symlink and
                # file all measured), or ELOOP, O_NOFOLLOW's errno in open(2).
                if error.errno not in (errno.ENOTDIR, errno.ELOOP):
                    raise
                try:
                    mode = os.stat(parts[depth - 1], dir_fd=parent, follow_symlinks=False).st_mode
                    what = kind(mode)
                except OSError:
                    what = 'no longer a directory'
                return 'structural', '/'.join(parts[:depth]) + ' is ' + what
            parent = child
        try:
            before = os.stat(parts[-1], dir_fd=parent, follow_symlinks=False)
        except FileNotFoundError:
            return 'remote', None
        if not (stat.S_ISREG(before.st_mode) or stat.S_ISLNK(before.st_mode)):
            return 'structural', path + ' is ' + kind(before.st_mode)
        return 'remote', digest(parent, parts[-1], before)
    except PermissionError:
        return 'held', 'the SSH user may not read it', True
    except ValueError:
        # digest's own stamp checks: the file moved while it was read.
        return 'held', 'it changed while it was read', False
    except OSError as error:
        return 'held', error.strerror or str(error), False
    except Exception as error:
        return 'held', '%s: %s' % (type(error).__name__, error), False


def load_cache(path):
    try:
        with open(path, 'rb') as source:
            data = json.load(source)
        entries = data['entries'] if data.get('version') == 1 else {}
        return entries if isinstance(entries, dict) else {}
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return {}


def trusted(entry, current):
    return (isinstance(entry, list) and len(entry) == 8
            and all(isinstance(x, int) for x in entry[:7]) and isinstance(entry[7], str)
            and entry[:6] == current and entry[6] - max(current[4], current[5]) >= SLACK_NS)


def save_cache(path, fresh, known, surveyed):
    """Merge what this call hashed into the cache. A list-mode call asks
    about a handful of names; writing only those would empty the cache for
    the next full survey (measured), and projects sharing the
    workspace share the file. Only a directory this call surveyed in full
    drops the entries it no longer holds."""
    merged = {name: entry for name, entry in known.items()
              if (name.rsplit('/', 1)[0] if '/' in name else '') not in surveyed}
    merged.update(fresh)
    fresh = merged
    if fresh == known:
        return
    try:
        os.makedirs(os.path.dirname(path), 0o700, exist_ok=True)
        temporary = path + '.tmp.' + str(os.getpid())
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as target:
            json.dump({'version': 1, 'entries': fresh}, target, separators=(',', ':'))
        os.replace(temporary, path)
    except OSError:
        pass


def digest_of(pairs):
    value = hashlib.sha256()
    for name, fingerprint in pairs:
        value.update(name.encode() + b'\0' + (fingerprint or '-').encode() + b'\n')
    return value.hexdigest()


class Inventory:
    def __init__(self, root, cache_path):
        self.cache_path = cache_path
        self.known = load_cache(cache_path) if cache_path else {}
        self.fresh = {}
        # Directories whose every name this call asked about (digest mode).
        self.surveyed = set()
        self.parents = Parents(root)

    def answer(self, path):
        hashed = time.time_ns()
        found = locate(self.parents, path)
        if found is None:
            return None
        parent, name, before = found
        if stat.S_ISDIR(before.st_mode):
            return 'd:' + str(stat.S_IMODE(before.st_mode))
        current = stamp(before)
        entry = self.known.get(path)
        if entry is not None and trusted(entry, current):
            self.fresh[path] = entry
            return entry[7]
        value = digest(parent, name, before)
        if value is not None:
            self.fresh[path] = current + [hashed, value]
        return value

    def close(self):
        self.parents.close()
        if self.cache_path:
            save_cache(self.cache_path, self.fresh, self.known, self.surveyed)


def run(root, cache_path):
    request = json.load(sys.stdin)
    inventory = Inventory(root, cache_path)
    try:
        if isinstance(request, list):
            result = {path: inventory.answer(path) for path in sorted(request)}
        elif 'verdicts' in request:
            result = {'remote': {}, 'structural': {}, 'held': {}}
            for path in sorted(request['verdicts']):
                answer = verdict(inventory.parents, path)
                if answer[0] == 'remote':
                    result['remote'][path] = answer[1]
                elif answer[0] == 'structural':
                    result['structural'][path] = answer[1]
                else:
                    result['held'][path] = {'why': answer[1], 'denied': answer[2]}
        else:
            differs = {}
            inventory.surveyed = set(request.get('dirs', {}))
            for directory, asked in sorted(request.get('dirs', {}).items()):
                pairs = [(name, inventory.answer(name if directory == '' else directory + '/' + name))
                         for name in asked['names']]
                if digest_of(pairs) != asked['digest']:
                    differs[directory] = dict(pairs)
            dirmodes = {}
            for directory, expected in sorted(request.get('dirmodes', {}).items()):
                actual = inventory.answer(directory)
                if actual != expected:
                    dirmodes[directory] = actual
            result = {'differs': differs, 'dirmodes': dirmodes}
    finally:
        inventory.close()
    print(json.dumps(result))


root = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
try:
    run(root, sys.argv[2] if len(sys.argv) > 2 else None)
finally:
    os.close(root)
