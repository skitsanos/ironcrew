"""Bounded subprocess/PTY capture with process-group cleanup on every path."""
import errno
import os
import signal
import subprocess
import threading
import time

from plan import CAPTURE_BYTES
from support import SmokeError, classify_failure, reject_secrets


class Deadline:
    def __init__(self, seconds):
        self.end = time.monotonic() + seconds

    def remaining(self):
        value = self.end - time.monotonic()
        if value <= 0:
            raise SmokeError("wall_time_limit")
        return value


class Process:
    def __init__(self, command, cwd, environment, canaries=(), driver=None):
        self.stdout = bytearray()
        self.stderr = bytearray()
        self.failure = None
        self.canaries = canaries
        self.threads = []
        self.master = None
        self.forced = False
        slave = None
        if driver is not None:
            import pty
            self.master, slave = pty.openpty()
        try:
            self.child = subprocess.Popen(command, cwd=cwd, env=environment,
                                          stdin=slave if slave is not None else subprocess.DEVNULL,
                                          stdout=subprocess.PIPE,
                                          stderr=slave if slave is not None else subprocess.PIPE,
                                          start_new_session=True)
        except BaseException:
            if self.master is not None:
                os.close(self.master)
            raise
        finally:
            if slave is not None:
                os.close(slave)
        self._start_reader(self.child.stdout.fileno(), self.stdout)
        stderr_fd = self.master if self.master is not None else self.child.stderr.fileno()
        self._start_reader(stderr_fd, self.stderr, driver)

    def _start_reader(self, descriptor, target, driver=None):
        def pump():
            pending = bytearray()
            try:
                while True:
                    chunk = os.read(descriptor, 4096)
                    if not chunk:
                        break
                    if len(target) + len(chunk) > CAPTURE_BYTES:
                        self.failure = "capture_limit"
                        continue  # Drain without retaining unbounded bytes.
                    target.extend(chunk)
                    if driver is not None:
                        pending.extend(chunk)
                        while b"> " in pending:
                            prompt, _, rest = pending.partition(b"> ")
                            pending = bytearray(rest)
                            if b"ask_human" not in prompt:
                                continue
                            text = prompt.decode("utf-8", "replace")
                            answer = driver.answer(text, "approval" if "[approval]" in text else "question")
                            os.write(self.master, (answer + "\n").encode())
                        if len(pending) > CAPTURE_BYTES:
                            raise SmokeError("capture_limit")
            except SmokeError as error:
                self.failure = error.code
            except OSError as error:
                if error.errno not in (errno.EIO, errno.EBADF):
                    self.failure = "capture_io_error"
        thread = threading.Thread(target=pump, daemon=True)
        thread.start()
        self.threads.append(thread)

    def check(self):
        if self.failure:
            raise SmokeError(self.failure)
        reject_secrets(bytes(self.stdout) + bytes(self.stderr), self.canaries)

    def wait(self, deadline):
        while self.child.poll() is None:
            self.check()
            time.sleep(min(0.02, deadline.remaining()))
        for thread in self.threads:
            thread.join(timeout=1)
        self.check()
        if self.child.returncode != 0:
            raise SmokeError(classify_failure(bytes(self.stdout) + bytes(self.stderr)))
        return bytes(self.stdout)

    def close(self):
        # The process group belongs exclusively to this start_new_session child.
        self.child.poll()  # Reap an already-exited leader before signaling (macOS).
        try:
            os.killpg(self.child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except PermissionError:
            # macOS can return EPERM while the just-exited group is disappearing.
            # An unexpectedly live leader still gets a bounded direct shutdown.
            if self.child.poll() is None:
                self.forced = True
                self.child.terminate()
        try:
            self.child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.forced = True
            os.killpg(self.child.pid, signal.SIGKILL)
            self.child.wait(timeout=3)
        # Also stop descendants that outlive an already-exited group leader.
        try:
            os.killpg(self.child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except PermissionError:
            if self.child.poll() is None:
                raise SmokeError("process_cleanup_failed") from None
        for thread in self.threads:
            thread.join(timeout=1)
        if self.master is not None:
            os.close(self.master)
        self.child.stdout.close()
        if self.child.stderr is not None:
            self.child.stderr.close()
