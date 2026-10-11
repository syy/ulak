"""Private protocol v1: bounded regular-file replaces, creates, unlinks and reads over SSH.

Selection, ownership and deletion POLICY belong to sync.rs: every item names a
leaf sync.rs chose, and this helper decides nothing but whether the server's
copy is still the one sync.rs expects: the last common content on the warm
path, the copy as surveyed on a full reconcile. A replace or unlink is
guarded by that expected content, compared even for a server copy too large
to send; a create needs an absent name under an existing parent. A batch is
verified in full before anything is applied, so a refusal applies nothing, and the receipt names what changed, what the server
lost and what was refused; sync.rs claims and retires on the receipt alone. A
read returns a small file's bytes after the receipt line, with the mode, mtime
and fingerprint they were read under and the modes of the directories above
it; sync.rs lands them under its own guard.
This helper cannot create or remove project directories. Directory descriptors
and O_NOFOLLOW keep a parent symlink from redirecting a selected write outside
the workspace.
"""
import json
import hashlib
import fcntl
import os
import secrets
import stat
import sys

MAX_FILE = 1024 * 1024
MAX_BATCH = 8 * MAX_FILE
MAX_HEADER = 65536
OPS = ("replace", "create", "unlink", "read")
reader = sys.stdin.buffer
writer = sys.stdout.buffer


def reply(value):
    writer.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
    writer.flush()


def stamp(st):
    return (st.st_dev, st.st_ino, st.st_size, st.st_mode,
            st.st_mtime_ns, st.st_ctime_ns)


def fingerprint(previous, st):
    return "f:" + str(stat.S_IMODE(st.st_mode)) + ":" + hashlib.sha256(previous).hexdigest()


def streamed(fd, st):
    """The fingerprint of a file too large to hold, read in chunks from fd."""
    digest = hashlib.sha256()
    while True:
        chunk = os.read(fd, 1 << 20)
        if not chunk:
            break
        digest.update(chunk)
    return "f:" + str(stat.S_IMODE(st.st_mode)) + ":" + digest.hexdigest()


class Item:
    def __init__(self, spec, data):
        self.spec = spec
        self.data = data
        self.op = spec.get("op", "replace")
        if self.op not in OPS:
            raise ValueError("unknown operation")
        self.path = spec["path"]
        self.parent = None
        self.name = None
        self.before = None
        self.previous = None
        self.parents = []

    def close(self):
        if self.parent is not None:
            os.close(self.parent)
            self.parent = None


def resolve(root, item):
    """The parent descriptor, opened component by component; None when a parent is gone."""
    parts = item.path.split("/")
    if any(p in ("", ".", "..") or "\x00" in p for p in parts):
        raise ValueError("invalid relative path")
    parent = os.dup(root)
    modes = []
    try:
        for part in parts[:-1]:
            next_parent = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                  dir_fd=parent)
            os.close(parent)
            parent = next_parent
            modes.append(stat.S_IMODE(os.fstat(parent).st_mode))
    except FileNotFoundError:
        # A parent that is now a file or a symlink is not a refusal but a
        # protocol failure: the channel ends and the full survey looks.
        os.close(parent)
        return
    item.parent = parent
    item.name = parts[-1]
    item.parents = modes


def verify(root, item):
    """The reason this item must be refused, or None. Nothing is written here."""
    resolve(root, item)
    if item.parent is None:
        # Only a create may meet a parent the server lacks: the survey saw
        # it absent, and rsync's creating pass makes it. For any other item
        # the parent went away since the survey — the server moved.
        if item.op == "create":
            return "missing parent"
        return None if item.op == "unlink" else "missing"
    try:
        item.before = os.stat(item.name, dir_fd=item.parent, follow_symlinks=False)
    except FileNotFoundError:
        item.before = None
    if item.op == "create" and item.before is None:
        return None
    if item.before is None:
        # An unlink of a name already gone is a deletion the server confirms.
        return None if item.op == "unlink" else "missing"
    if not stat.S_ISREG(item.before.st_mode):
        return "not a regular file"
    large = item.before.st_size > MAX_FILE
    if large and item.op == "read":
        return "needs rsync"
    # A large server copy is still compared before anything is decided:
    # a file that grew past the limit since the survey is a server that
    # moved, and answering "needs rsync" first would hand sync.rs a stale
    # selection to overwrite it with (measured). The replace
    # itself writes only the small local bytes, so it needs no rsync.
    try:
        fd = os.open(item.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=item.parent)
    except PermissionError:
        # A copy the SSH user may not read cannot be compared, so nothing
        # is decided here; ending the channel instead would fail every
        # warm batch that names it until the file became readable.
        return "unreadable"
    with os.fdopen(fd, "rb") as old:
        if stamp(os.fstat(old.fileno())) != stamp(item.before):
            return "changed while reading"
        if large:
            current = streamed(old.fileno(), item.before)
        else:
            item.previous = old.read(MAX_FILE + 1)
            current = fingerprint(item.previous, item.before)
        if stamp(os.fstat(old.fileno())) != stamp(item.before):
            return "changed while reading"
    if item.op == "read":
        item.spec["content"] = current
        return None
    if item.op == "create":
        # A name that already holds these bytes — server output that came
        # home and is now being claimed — is replaced in place like any
        # synced file; any other content under it is a conflict.
        if current != item.spec["content"]:
            return "exists"
        item.op = "replace"
        return None
    if item.op == "unlink":
        if item.spec.get("expected") is None:
            return "unknown ancestor"
        return None if current == item.spec["expected"] else "differs"
    if item.spec.get("guarded") and current not in (item.spec.get("expected"), item.spec["content"]):
        return "differs"
    return None


def apply(control, lock, lease, item):
    """Apply one verified item under the lease; True when the server changed."""
    parent, name, before, data = item.parent, item.name, item.before, item.data
    if item.op == "unlink":
        if before is None:
            return True
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            os.stat(lease, dir_fd=control, follow_symlinks=False)
            if stamp(os.stat(name, dir_fd=parent, follow_symlinks=False)) != stamp(before):
                raise ValueError("destination changed before removal")
            os.unlink(name, dir_fd=parent)
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
        return True
    mode = item.spec["mode"] & 0o7777
    modified = item.spec["modified"][0] * 1000000000 + item.spec["modified"][1]
    if (item.op == "replace" and item.previous == data
            and stat.S_IMODE(before.st_mode) == mode and before.st_mtime_ns == modified):
        return False
    # Use rsync's already-excluded staging directory, on the same mount.
    # A killed helper's partial bytes must never become remote output.
    temporary = None
    staging = None
    created_staging = False
    try:
        try:
            os.mkdir(".ulak-partial", 0o700, dir_fd=parent)
            created_staging = True
        except FileExistsError:
            pass
        staging = os.open(".ulak-partial", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                          dir_fd=parent)
        temporary = secrets.token_hex(12)
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=staging)
        with os.fdopen(fd, "wb") as out:
            out.write(data)
            out.flush()
            os.fchmod(out.fileno(), mode)
            os.utime(out.fileno(), ns=(before.st_atime_ns if before else modified, modified))
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            os.stat(lease, dir_fd=control, follow_symlinks=False)
            if item.op == "create":
                # link() refuses an existing name, so a file that appeared
                # since verification is never overwritten.
                os.link(temporary, name, src_dir_fd=staging, dst_dir_fd=parent)
            else:
                if stamp(os.stat(name, dir_fd=parent, follow_symlinks=False)) != stamp(before):
                    raise ValueError("destination changed before replacement")
                os.replace(temporary, name, src_dir_fd=staging, dst_dir_fd=parent)
                temporary = None
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
        return True
    finally:
        if temporary is not None:
            os.unlink(temporary, dir_fd=staging)
        if staging is not None:
            os.close(staging)
        if created_staging:
            try:
                os.rmdir(".ulak-partial", dir_fd=parent)
            except OSError:
                pass


def serve(root, control, lock, lease, request):
    files = request["files"]
    if not 0 < len(files) <= 64:
        raise ValueError("invalid batch count")
    sizes = [item["size"] for item in files]
    if any(type(n) is not int or not 0 <= n <= MAX_FILE for n in sizes) or sum(sizes) > MAX_BATCH:
        raise ValueError("payload exceeds protocol limit")
    items = []
    for spec in files:
        data = reader.read(spec["size"])
        if len(data) != spec["size"]:
            raise EOFError("incomplete payload")
        items.append(Item(spec, data))
    changed, deleted, refused, reads, payload = [], [], [], [], []
    read_total = 0
    try:
        for item in items:
            reason = verify(root, item)
            if reason is None and item.op == "read":
                # The receipt's reads share the batch limit the client
                # enforces; past it, the pull's rsync path takes the batch
                # instead of the client rejecting the whole receipt.
                read_total += len(item.previous)
                if read_total > MAX_BATCH:
                    reason = "needs rsync"
            if reason is not None:
                refused.append({"path": item.path, "reason": reason})
        if not refused:
            for item in items:
                if item.op == "read":
                    reads.append({"path": item.path, "size": len(item.previous),
                                  "mode": stat.S_IMODE(item.before.st_mode),
                                  "modified": [item.before.st_mtime_ns // 1000000000,
                                               item.before.st_mtime_ns % 1000000000],
                                  "content": item.spec["content"],
                                  "parents": item.parents})
                    payload.append(item.previous)
                elif item.op == "unlink":
                    apply(control, lock, lease, item)
                    deleted.append(item.path)
                elif apply(control, lock, lease, item):
                    changed.append(item.path)
    finally:
        for item in items:
            item.close()
    reply({"version": 1, "id": request["id"], "changed": changed, "deleted": deleted,
           "refused": refused, "reads": reads, "error": None})
    for data in payload:
        writer.write(data)
    writer.flush()


def run():
    root = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.mkdir(sys.argv[2], 0o700)
    except FileExistsError:
        pass
    control = os.open(sys.argv[2], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    lock = os.open("stream.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600, dir_fd=control)
    lease = "stream-" + secrets.token_hex(16) + ".lease"
    fcntl.flock(lock, fcntl.LOCK_EX)
    try:
        os.close(os.open(lease, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=control))
    finally:
        fcntl.flock(lock, fcntl.LOCK_UN)
    try:
        reply({"version": 1, "id": 0, "changed": [], "deleted": [], "refused": [], "error": None})
        while True:
            line = reader.readline(MAX_HEADER + 1)
            if not line:
                return
            if len(line) > MAX_HEADER or not line.endswith(b"\n"):
                raise ValueError("header exceeds protocol limit")
            serve(root, control, lock, lease, json.loads(line))
    finally:
        try:
            os.unlink(lease, dir_fd=control)
        except FileNotFoundError:
            pass
        os.close(lock)
        os.close(control)
        os.close(root)


try:
    run()
except Exception as error:
    reply({"version": 1, "id": -1, "changed": [], "deleted": [], "refused": [], "error": str(error)})
