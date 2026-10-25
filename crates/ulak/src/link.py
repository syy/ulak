"""Ulak's link helper: one process on the server that runs every command a
channel of the shared ssh connection brings it.

link.rs owns the decision and the reasons; this file owns the far end. A
channel is one connection to the helper's unix socket, forwarded by the
local ssh master (`ssh -O forward -L`). It is not an ssh session, so sshd's
MaxSessions does not count it, and the master relays its bytes instead of
holding the caller's descriptors.

Protocol v1, one command per channel. Every frame is a type byte, a 4-byte
big-endian length and that many bytes. The helper speaks first (H, a JSON
hello, with "full" when it already runs CHANNELS commands and takes no
request); the client answers with one O (a JSON request: the command line
for /bin/sh -c, and an optional pty). Then the client sends 0 (stdin bytes),
0 with no bytes (stdin's end), K (a signal by name) and W (a window size);
the helper sends 1 and 2 (stdout and stderr bytes), then X (a JSON exit:
"code", or "signal" by name) once the command has ended and its output is
drained, or F (a reason, UTF-8) when it could not start it at all. Drained
is sshd's rule: a pipe is read until its last writer closes it, a pty for
DRAIN seconds after its command ends, and then its master is closed; what
the terminal holds by then is sent however slowly the client reads it.

A channel that closes before X is a caller that went away: the command's
whole process group gets SIGHUP, then SIGKILL after GRACE. Nothing outlives
the client that asked for it.

A helper serves one ssh connection: its name carries the connection's
SSH_CONNECTION, so its commands have that login's environment, groups and
forwarded agent, and a new connection gets a new helper, as a new ssh
session would. It takes new channels for LIFE seconds after it starts (the
master's ControlPersist), then gives its socket up and leaves once its last
command has ended; it gives it up sooner when its name no longer leads to it,
and when a login now would get other groups than it has (Linux), so that
`usermod -aG docker` reaches the next command: `start` says `stale` in a
login that lacks a group, and the client logs in again. Other changes to the
login environment reach commands within LIFE.

Nothing it starts outlives it either: a helper that cannot go on, or is
told to stop with TERM, hangs every command it runs up first. A traceback
goes to NAME.log beside its socket. `start` is the only way in: it finds a
private directory, starts a helper there unless one already answers, and
prints the socket path.
"""
import binascii
import errno
import fcntl
import json
import os
import platform
import pwd
import queue
import select
import signal
import socket
import stat
import struct
import subprocess
import sys
import threading
import time
import traceback

VERSION = 1
HEADER = struct.Struct(">BI")
# An O carries a whole command line, and some of ulak's carry a helper's
# source as an argument; data frames are never larger than CHUNK.
MAX_FRAME = 1 << 24
CHUNK = 1 << 16
GRACE = 5
DRAIN = 1
# At most this much of a pty's output after DRAIN: what the command left in
# the terminal, not what something left behind goes on writing into it.
TAIL = 1 << 20
CHANNELS = 256
# Each command holds four descriptors here: its channel and three pipes.
PER_CHANNEL = 4
LOG_KEPT = 1 << 16
# The command a pty child runs first: take the terminal as its own, then
# become the real command. Popen cannot do the first step for us.
CTTY = ("import fcntl,os,sys,termios;fcntl.ioctl(0,termios.TIOCSCTTY,0);"
        "os.execv(sys.argv[1],sys.argv[1:])")


def frame(kind, payload=b""):
    return HEADER.pack(ord(kind), len(payload)) + payload


def exactly(conn, size):
    data = bytearray()
    while len(data) < size:
        chunk = conn.recv(size - len(data))
        if not chunk:
            return None
        data += chunk
    return bytes(data)


class Peer:
    """The channel's two directions. Sends are whole frames under one lock;
    a send that fails marks the client gone and every later one is dropped."""

    def __init__(self, conn):
        self.conn = conn
        self.lock = threading.Lock()
        self.gone = threading.Event()

    def send(self, kind, payload=b""):
        if self.gone.is_set():
            return False
        try:
            with self.lock:
                self.conn.sendall(frame(kind, payload))
            return True
        except OSError:
            self.gone.set()
            return False

    def recv(self):
        try:
            head = exactly(self.conn, HEADER.size)
            if head is None:
                return None
            kind, size = HEADER.unpack(head)
            if size > MAX_FRAME:
                return None
            payload = exactly(self.conn, size) if size else b""
            if payload is None:
                return None
            return chr(kind), payload
        except OSError:
            return None


def signal_named(name):
    if not (name.isalpha() and name.isupper()):
        return None
    value = getattr(signal, "SIG" + name, None)
    return None if value is None else int(value)


def signal_name(number):
    try:
        return signal.Signals(number).name[3:]
    except ValueError:
        return str(number)


def winsize(fd, size):
    rows = int(size.get("rows") or 24)
    cols = int(size.get("cols") or 80)
    import termios
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


class Command:
    """One request's process and the threads that carry its bytes."""

    def __init__(self, peer, request):
        self.peer = peer
        self.pty = request.get("pty")
        self.inbox = queue.Queue(maxsize=16)
        self.pumps = []
        self.done = threading.Event()
        self.closing = threading.Event()
        self.cut = threading.Event()
        self.stdin_ended = False
        env = dict(os.environ)
        if self.pty and self.pty.get("term"):
            env["TERM"] = str(self.pty["term"])
        line = request["sh"]
        if self.pty:
            master, slave = os.openpty()
            try:
                winsize(master, self.pty)
                self.child = subprocess.Popen(
                    [sys.executable, "-c", CTTY, "/bin/sh", "-c", line],
                    stdin=slave, stdout=slave, stderr=slave,
                    start_new_session=True, env=env)
            except BaseException:
                os.close(master)
                raise
            finally:
                os.close(slave)
            self.master = master
            self.stdin = master
            self.outputs = [(master, "1")]
        else:
            self.master = None
            self.child = subprocess.Popen(
                ["/bin/sh", "-c", line],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, start_new_session=True, env=env)
            self.stdin = self.child.stdin.fileno()
            self.outputs = [(self.child.stdout.fileno(), "1"),
                            (self.child.stderr.fileno(), "2")]
        self.feeder = threading.Thread(target=self.feed, daemon=True)

    def start(self):
        for fd, kind in self.outputs:
            pump = threading.Thread(target=self.pump, args=(fd, kind), daemon=True)
            pump.start()
            self.pumps.append(pump)
        self.feeder.start()
        threading.Thread(target=self.finish, daemon=True).start()

    def pump(self, fd, kind):
        # A client that left still has its command's output drained, so the
        # command is never stopped by a full pipe before the hangup reaches it.
        # Only a pty is cut; a pipe is read until its last writer closes.
        poller = select.poll() if self.master is not None else None
        if poller is not None:
            poller.register(fd, select.POLLIN)
        while True:
            if poller is not None:
                while not self.cut.is_set() and not poller.poll(200):
                    pass
            if self.cut.is_set():
                self.rest(fd, kind, poller)
                return
            try:
                data = os.read(fd, CHUNK)
            except OSError as e:
                if e.errno == errno.EINTR:
                    continue
                break  # EIO: a pty whose last slave closed
            if not data:
                break
            self.peer.send(kind, data)

    def rest(self, fd, kind, poller):
        left = TAIL
        while left > 0 and poller.poll(0):
            try:
                data = os.read(fd, min(CHUNK, left))
            except OSError:
                return
            if not data:
                return
            left -= len(data)
            self.peer.send(kind, data)

    def feed(self):
        while not self.closing.is_set():
            try:
                data = self.inbox.get(timeout=0.5)
            except queue.Empty:
                continue
            if data is None:
                if self.master is None:
                    try:
                        self.child.stdin.close()
                    except OSError:
                        pass
                return
            try:
                view = memoryview(data)
                while view and not self.closing.is_set():
                    view = view[os.write(self.stdin, view):]
            except OSError:
                # The command closed its stdin; what follows is discarded,
                # as a pipe would.
                pass

    def finish(self):
        code = self.child.wait()
        # A pipe's reader waits for every writer, as sshd does. A pty ends
        # with the command, as sshd ends one: whatever it left in the
        # background with the terminal gets a moment to drain, and then
        # loses the terminal when its master is closed below.
        if self.master is not None:
            for pump in self.pumps:
                pump.join(DRAIN)
            self.cut.set()
        for pump in self.pumps:
            pump.join()
        # Marked before it is said: a client that read X may close at once,
        # and must not be taken for one that left and hang up what the
        # command left running on purpose.
        self.done.set()
        if code < 0:
            exit = {"signal": signal_name(-code), "number": -code}
            self.peer.send("X", json.dumps(exit).encode())
        else:
            self.peer.send("X", json.dumps({"code": code}).encode())
        # Both ways: what the client still sends is stdin nobody reads, and
        # the channel's end does not wait on a client that keeps it open.
        try:
            self.peer.conn.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        # Descriptors are closed only once no thread can still use them: a
        # number closed under a writer is a number the next channel reuses.
        self.closing.set()
        self.feeder.join(GRACE)
        if self.feeder.is_alive() or any(pump.is_alive() for pump in self.pumps):
            return
        if self.master is not None:
            os.close(self.master)
        else:
            for pipe in (self.child.stdin, self.child.stdout, self.child.stderr):
                try:
                    pipe.close()
                except OSError:
                    pass

    def deliver(self, data):
        """Queue stdin for the command. The queue is bounded, so stdin keeps
        its backpressure; while it is full, the channel is still watched so
        a client that left is seen (Linux: POLLRDHUP), and False says so.
        Once the command has ended the rest of stdin is dropped, and the
        channel kept: its exit status is still on the way."""
        if self.stdin_ended:
            return True
        if data is None:
            self.stdin_ended = True
        while True:
            try:
                self.inbox.put(data, timeout=0.5)
                return True
            except queue.Full:
                if self.left():
                    return False
                if self.child.poll() is not None:
                    self.stdin_ended = True
                    return True

    def left(self):
        hup = getattr(select, "POLLRDHUP", 0)
        if not hup:
            return False
        poller = select.poll()
        poller.register(self.peer.conn.fileno(), hup)
        return bool(poller.poll(0))

    def signal(self, name):
        number = signal_named(name)
        if number is None:
            return
        try:
            os.killpg(self.child.pid, number)
        except OSError:
            pass

    def resize(self, payload):
        if self.master is None:
            return
        try:
            winsize(self.master, json.loads(payload.decode()))
        except (OSError, ValueError, TypeError, AttributeError):
            pass

    def hangup(self):
        """The client went away first: the group gets SIGHUP, then SIGKILL."""
        if self.child.poll() is not None and not self.group_alive():
            return
        try:
            os.killpg(self.child.pid, signal.SIGHUP)
        except OSError:
            return
        deadline = time.monotonic() + GRACE
        while time.monotonic() < deadline:
            if self.child.poll() is not None and not self.group_alive():
                return
            time.sleep(0.1)
        try:
            os.killpg(self.child.pid, signal.SIGKILL)
        except OSError:
            pass

    def group_alive(self):
        try:
            os.killpg(self.child.pid, 0)
            return True
        except OSError:
            return False


def channel(conn, server):
    peer = Peer(conn)
    command = None
    try:
        hello = {"v": VERSION, "pid": os.getpid(), "python": platform.python_version()}
        # Said before the request, so the client can still take the command
        # to ssh: nothing has run.
        full = server.busy() > server.channels
        if full:
            hello["full"] = True
        if not peer.send("H", json.dumps(hello).encode()) or full:
            return
        # A signal or a resize can race the request it is meant for; it is
        # the command's as soon as there is one.
        pending = []
        while True:
            got = peer.recv()
            if got is None:
                return
            if got[0] == "O":
                break
            if got[0] not in "KW":
                return
            pending.append(got)
        try:
            request = json.loads(got[1].decode())
            if not isinstance(request.get("sh"), str):
                raise ValueError("the request names no command")
            command = Command(peer, request)
        except (OSError, ValueError, TypeError, AttributeError, UnicodeDecodeError) as e:
            peer.send("F", str(e).encode())
            return
        server.running(command, True)
        try:
            command.start()
        except RuntimeError:
            # No thread to carry its streams: hung up, and said so.
            command.hangup()
            peer.send("F", b"the helper could not start a thread for the command")
            return
        for kind, payload in pending:
            if kind == "K":
                command.signal(payload.decode("ascii", "replace"))
            else:
                command.resize(payload)
        while True:
            got = peer.recv()
            if got is None:
                break
            kind, payload = got
            if kind == "0":
                if not command.deliver(payload if payload else None):
                    break
            elif kind == "K":
                command.signal(payload.decode("ascii", "replace"))
            elif kind == "W":
                command.resize(payload)
        # Closed after X is a client that is done; before it, one that left.
        if not command.done.is_set():
            command.hangup()
        command.closing.set()
    finally:
        try:
            conn.close()
        except OSError:
            pass
        if command is not None:
            server.running(command, False)
        server.left()


class Server:
    def __init__(self, path, life):
        self.path = path
        self.retire_at = time.monotonic() + life
        self.wake = min(5.0, max(life, 0.2))
        self.lock = threading.Lock()
        self.active = 0
        self.commands = set()
        self.channels = channels()
        self.ready = None
        self.login = login_groups()
        self.groups_at = time.monotonic()
        self.went_stale = False

    def busy(self):
        with self.lock:
            return self.active

    def joined(self):
        with self.lock:
            self.active += 1

    def left(self):
        with self.lock:
            self.active -= 1

    def running(self, command, yes):
        with self.lock:
            if yes:
                self.commands.add(command)
            else:
                self.commands.discard(command)

    def serve(self):
        old = signal.signal(signal.SIGTERM, stopped)
        try:
            self.listen()
        except BaseException:
            self.abandon()
            raise
        finally:
            signal.signal(signal.SIGTERM, old)

    def listen(self):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        old = os.umask(0o177)
        try:
            listener.bind(self.path)
        finally:
            os.umask(old)
        listener.listen(128)
        inode = os.stat(self.path).st_ino
        if self.ready is not None:
            os.write(self.ready, b"ok")
            os.close(self.ready)
            self.ready = None
        listener.settimeout(self.wake)
        accepting = True
        while True:
            if accepting:
                try:
                    conn, _ = listener.accept()
                except socket.timeout:
                    conn = None
                except OSError:
                    # Out of descriptors or memory for now. The client waits
                    # in the backlog, and every command running goes on.
                    time.sleep(0.1)
                    conn = None
                if conn is not None and self.stale():
                    # A login now would get other groups: closed unanswered,
                    # before anything ran, and the client starts the next
                    # helper with them.
                    conn.close()
                    conn = None
                if conn is not None:
                    conn.settimeout(None)
                    self.joined()
                    try:
                        threading.Thread(target=channel, args=(conn, self), daemon=True).start()
                    except RuntimeError:
                        # No thread to serve it: closed unanswered, before
                        # anything ran, which the client takes to ssh.
                        self.left()
                        conn.close()
                    if time.monotonic() < self.retire_at and not self.stale():
                        continue
            else:
                time.sleep(self.wake)
            if accepting and (time.monotonic() >= self.retire_at or self.stale()
                              or not self.ours(inode)):
                # The name goes before the listener: until the listener
                # closes no `start` can see this helper as gone, so the
                # name unlinked here is still this helper's own. A client
                # still in the backlog sees the channel close unanswered,
                # before anything ran, and starts the next helper.
                if self.ours(inode):
                    try:
                        os.unlink(self.path)
                    except OSError:
                        pass
                listener.close()
                accepting = False
            if not accepting and self.busy() == 0:
                return

    def ours(self, inode):
        try:
            st = os.lstat(self.path)
        except OSError:
            return False
        return stat.S_ISSOCK(st.st_mode) and st.st_ino == inode

    def stale(self):
        """Whether a login now would get other groups than the one that
        started this helper. Asked at most once a second, and once so,
        always so."""
        if self.went_stale or self.login is None or time.monotonic() - self.groups_at < 1:
            return self.went_stale
        self.groups_at = time.monotonic()
        name, gid, groups = self.login
        try:
            self.went_stale = set(os.getgrouplist(name, gid)) != groups
        except (OSError, KeyError):
            pass
        return self.went_stale

    def abandon(self):
        """The helper cannot go on: what it started does not outlive it."""
        with self.lock:
            groups = [command.child.pid for command in self.commands]
        for pid in groups:
            try:
                os.killpg(pid, signal.SIGHUP)
            except OSError:
                pass
        deadline = time.monotonic() + GRACE
        while time.monotonic() < deadline and any(group_alive(pid) for pid in groups):
            time.sleep(0.1)
        for pid in groups:
            try:
                os.killpg(pid, signal.SIGKILL)
            except OSError:
                pass


def stopped(number, frame):
    raise SystemExit(128 + number)


def group_alive(pid):
    try:
        os.killpg(pid, 0)
        return True
    except OSError:
        return False


def channels():
    """As many commands as this process has descriptors for, at most
    CHANNELS: the limit is met by refusing in the hello, not by an accept
    that fails."""
    try:
        import resource
        soft = resource.getrlimit(resource.RLIMIT_NOFILE)[0]
    except (ImportError, ValueError, OSError):
        return CHANNELS
    if soft == resource.RLIM_INFINITY:
        return CHANNELS
    return max(1, min(CHANNELS, (soft - 64) // PER_CHANNEL))


def login_groups():
    """This account's groups as a login gets them now (Linux, where they
    are this process's from its start). None where that cannot be asked."""
    if not sys.platform.startswith("linux") or not hasattr(os, "getgrouplist"):
        return None
    try:
        entry = pwd.getpwuid(os.getuid())
        return entry.pw_name, entry.pw_gid, set(os.getgrouplist(entry.pw_name, entry.pw_gid))
    except (OSError, KeyError):
        return None


def login_lacks_groups():
    """Whether a login now gets a group this process lacks. Only gains
    count: a login may have more groups than the account lists (pam_group)."""
    login = login_groups()
    if login is None:
        return False
    try:
        have = set(os.getgroups()) | {os.getegid()}
    except OSError:
        return False
    return not login[2] <= have


def answers(path):
    """Whether a helper of this version is serving `path` right now."""
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(3)
    try:
        conn.connect(path)
        got = Peer(conn).recv()
        if got is None or got[0] != "H":
            return False
        return json.loads(got[1].decode()).get("v") == VERSION
    except (OSError, ValueError):
        return False
    finally:
        conn.close()


def private_dir():
    """The first directory only this user can use, short enough for a
    socket path. Each candidate is refused, not repaired, when someone else
    owns it or it is not a plain directory."""
    uid = os.getuid()
    candidates = []
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime:
        candidates.append(os.path.join(runtime, "ulak"))
    candidates.append("/tmp/ulak-%d" % uid)
    candidates.append(os.path.join(os.path.expanduser("~"), ".ulak", "run"))
    for path in candidates:
        # sun_path holds 104 bytes on macOS: this, a NAME and ".sock".
        if len(path) > 60:
            continue
        try:
            os.makedirs(path, mode=0o700, exist_ok=True)
            st = os.lstat(path)
            if not stat.S_ISDIR(st.st_mode) or st.st_uid != uid:
                continue
            if stat.S_IMODE(st.st_mode) & 0o077:
                os.chmod(path, 0o700)
            return path
        except OSError:
            continue
    return None


def start(name, life):
    directory = private_dir()
    if directory is None:
        print("ulak-link refused: no private directory for the socket")
        return 1
    connection = os.environ.get("SSH_CONNECTION")
    if connection:
        name += "-%08x" % (binascii.crc32(connection.encode()) & 0xffffffff)
    path = os.path.join(directory, name + ".sock")
    # Before `answers`: a helper of this login lacks the group as well.
    if login_lacks_groups():
        print("ulak-link stale")
        return 0
    lock = os.open(os.path.join(directory, name + ".lock"),
                   os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    fcntl.flock(lock, fcntl.LOCK_EX)
    if answers(path):
        print("ulak-link ready " + path)
        return 0
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    sys.stdout.flush()
    readable, writable = os.pipe()
    pid = os.fork()
    if pid == 0:
        server = None
        try:
            os.close(readable)
            os.close(lock)
            os.setsid()
            if os.fork():
                os._exit(0)
            # The session that started us must be free to end.
            null = os.open(os.devnull, os.O_RDWR)
            log = open_log(os.path.join(directory, name + ".log"), null)
            for fd, to in ((0, null), (1, null), (2, log)):
                os.dup2(to, fd)
            os.chdir(os.path.expanduser("~"))
            server = Server(path, life)
            server.ready = writable
            server.serve()
        except BaseException as e:
            if server is None or server.ready is not None:
                try:
                    os.write(writable, ("failed: %s" % e).encode()[:200])
                except OSError:
                    pass
            elif not isinstance(e, SystemExit):
                traceback.print_exc()
        finally:
            os._exit(0)
    os.close(writable)
    os.waitpid(pid, 0)
    said = os.read(readable, 256)
    if said != b"ok":
        print("ulak-link refused: " + (said.decode("utf-8", "replace") or "the helper did not start"))
        return 1
    print("ulak-link ready " + path)
    return 0


def open_log(path, fallback):
    """NAME.log, appended to and cut back once it is past LOG_KEPT."""
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND
                     | getattr(os, "O_NOFOLLOW", 0), 0o600)
    except OSError:
        return fallback
    try:
        if os.fstat(fd).st_size > LOG_KEPT:
            os.ftruncate(fd, 0)
    except OSError:
        pass
    return fd


def main(argv):
    if len(argv) >= 3 and argv[0] == "start":
        return start(argv[1], float(argv[2]))
    if len(argv) >= 3 and argv[0] == "serve":
        Server(argv[1], float(argv[2])).serve()
        return 0
    print("usage: start NAME LIFE | serve PATH LIFE")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
