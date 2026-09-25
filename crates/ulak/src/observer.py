"""Read-only Linux change feed. Policy is supplied by sync.rs as literal roots/prunes.

Watch before listing children: files created during registration remain queued.
Overflow, moved watches, unsupported files and inaccessible paths invalidate the
index instead of certifying silence. A metadata audit also catches mmap writes
and changes through hard links, which inotify(7) cannot reliably report.
"""
import ctypes
import hashlib
import json
import os
import select
import stat
import struct
import sys
import time

LIMIT = 4096
MASK = 0x2 | 0x4 | 0x8 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400 | 0x800


def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


def stamp(s):
    return s.st_dev, s.st_ino, s.st_mode, s.st_size, s.st_mtime_ns, s.st_ctime_ns


def run():
    os.chdir(sys.argv[1])
    config = json.loads(sys.argv[2])
    max_file = config["max_file"]
    roots = [p.removeprefix("./") for p in config["roots"]]
    pruned = config["pruned"]
    libc = ctypes.CDLL(None, use_errno=True)
    libc.inotify_init1.argtypes = [ctypes.c_int]
    libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
    fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
    if fd < 0:
        raise OSError(ctypes.get_errno(), "inotify_init1")
    watches = {}
    known = {}
    pending = set()

    def excluded(path):
        return ".ulak-partial" in path.split("/") or any(
            path == p or path.startswith(p.rstrip("/") + "/") for p in pruned)

    def selected(path):
        return not excluded(path) and any(
            r == "." or path == r or path.startswith(r.rstrip("/") + "/") for r in roots)

    def enqueue(path):
        if selected(path):
            path.encode("utf-8", "strict")
            if "\n" in path or "\r" in path:
                raise ValueError("unsafe filename")
            pending.add(path)
            if len(pending) > LIMIT:
                raise ValueError("change queue overflow")

    def scan(directory, initial=False):
        # IN_ONLYDIR|IN_DONT_FOLLOW refuse a symlink at registration.
        wd = libc.inotify_add_watch(fd, os.fsencode(directory), MASK | 0x01000000 | 0x02000000)
        if wd < 0:
            raise OSError(ctypes.get_errno(), "inotify_add_watch", directory)
        if wd in watches and watches[wd] != directory:
            raise ValueError("directory alias or rename")
        watches[wd] = directory
        with os.scandir(directory) as entries:
            for entry in entries:
                path = entry.path.removeprefix("./")
                if excluded(path):
                    continue
                if entry.is_dir(follow_symlinks=False):
                    if selected(path) or any(r.startswith(path + "/") for r in roots):
                        scan(path, initial)
                elif selected(path):
                    current = stamp(entry.stat(follow_symlinks=False))
                    if not initial and known.get(path) != current:
                        enqueue(path)
                    known[path] = current

    # Ancestors are watched too: replacing a footprint root invalidates the
    # watch mapping, rather than leaving a live descriptor on a detached tree.
    scan(".", True)
    emit({"ready": True})
    audit_due = time.monotonic() + 60
    flush_due = time.monotonic() + .05
    while True:
        ready, _, _ = select.select([fd, sys.stdin.fileno()], [], [], .05)
        if sys.stdin.fileno() in ready:
            if not os.read(sys.stdin.fileno(), 1):
                return
            raise ValueError("unexpected watcher input")
        if fd in ready:
            data = os.read(fd, 256 * 1024)
            offset = 0
            while offset < len(data):
                wd, mask, _, length = struct.unpack_from("iIII", data, offset)
                name = os.fsdecode(data[offset + 16:offset + 16 + length].split(b"\0", 1)[0])
                offset += 16 + length
                if mask & (0x4000 | 0x8000 | 0x400 | 0x800 | 0x2000):
                    raise ValueError("watch lost, moved, unmounted or overflowed")
                parent = watches.get(wd)
                if parent is None:
                    raise ValueError("unknown watch")
                path = os.path.join(parent, name).removeprefix("./")
                if excluded(path):
                    continue
                if mask & 0x40000000:
                    if mask & (0x40 | 0x200):
                        raise ValueError("directory removed or moved")
                    if mask & (0x80 | 0x100):
                        if selected(path) or any(r.startswith(path + "/") for r in roots):
                            scan(path)
                elif name:
                    enqueue(path)
        now = time.monotonic()
        if now >= audit_due:
            # Scanning is in this read-only helper; it cannot stall local
            # writes on the separate persistent transfer channel.
            scan(".")
            for path in list(known):
                if not os.path.lexists(path):
                    enqueue(path)
            audit_due = now + 60
        if now >= flush_due:
            for path in sorted(pending)[:64]:
                pending.remove(path)
                try:
                    before = os.stat(path, follow_symlinks=False)
                except FileNotFoundError:
                    # Reconcile restores a locally owned file; it never
                    # interprets this notice as permission to delete locally.
                    # A vanished editor/rsync temporary name was never indexed.
                    if known.pop(path, None) is not None:
                        emit({"path": path, "deleted": True})
                    continue
                if not stat.S_ISREG(before.st_mode) or before.st_size > max_file:
                    emit({"path": path})
                    continue
                f = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                with os.fdopen(f, "rb") as source:
                    opened = os.fstat(source.fileno())
                    if stamp(opened) != stamp(before):
                        enqueue(path)
                        continue
                    digest = hashlib.sha256(source.read(max_file + 1)).hexdigest()
                    after = os.fstat(source.fileno())
                if stamp(after) != stamp(before) or stamp(os.stat(path, follow_symlinks=False)) != stamp(before):
                    enqueue(path)
                    continue
                known[path] = stamp(before)
                emit({"path": path, "digest": digest, "mode": stat.S_IMODE(before.st_mode),
                      "modified": [before.st_mtime_ns // 1000000000, before.st_mtime_ns % 1000000000]})
            flush_due = now + .05


try:
    run()
except Exception as error:
    emit({"error": str(error)})
