"""Private file control and lifecycle for the release fixture subprocess."""

from contextlib import suppress
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import time


FIXTURE_TEST = "server::output_tests::r13_acceptance::fixture_host"


class AcceptanceError(Exception):
    """An acceptance contract or fixture invariant failed."""


def require(condition, message):
    if not condition:
        raise AcceptanceError(message)


def file_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def private_json(path, value):
    pending = path.with_suffix(path.suffix + ".writing")
    with os.fdopen(os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        json.dump(value, stream, ensure_ascii=False, separators=(",", ":"))
        stream.write("\n")
    pending.replace(path)


def read_private_json(path):
    metadata = path.lstat()
    require(stat.S_ISREG(metadata.st_mode), f"{path.name} must be a regular file")
    require(stat.S_IMODE(metadata.st_mode) == 0o600, f"{path.name} must have mode 0600")
    require(metadata.st_size <= 128 * 1024, f"{path.name} exceeds its control budget")
    return json.loads(path.read_bytes())


class Fixture:
    def __init__(self, binary, directory, log=None):
        self.binary = binary
        self.root = directory
        self.process = None
        self.sequence = 0
        self.ready = None
        self.log = log or directory.parent / "fixture.log"

    def __enter__(self):
        require(stat.S_IMODE(self.root.stat().st_mode) == 0o700, "fixture directory must have mode 0700")
        env = {"R13_DIR": str(self.root)}
        with os.fdopen(os.open(self.log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as stream:
            self.process = subprocess.Popen(
                [str(self.binary), "--exact", FIXTURE_TEST, "--ignored", "--nocapture"],
                cwd=self.root,
                env=env,
                stdout=stream,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        try:
            self.ready = self._wait("ready.json", lambda data: data.get("schema") == 1, 45)
            self._validate_paths()
        except BaseException:
            self._kill()
            raise
        return self

    def _validate_paths(self):
        for key in ("cache_root", "stage_root", "operations_db"):
            path = Path(self.ready[key])
            require(path.is_absolute() and path.resolve().is_relative_to(self.root), f"{key} escapes the fixture")
        pid = self.ready["service_pid"]
        require(isinstance(pid, int) and pid > 0 and pid != self.process.pid, "missing separate service PID")
        status = Path(f"/proc/{pid}/status").read_text()
        parent = next(line.split()[1] for line in status.splitlines() if line.startswith("PPid:"))
        require(int(parent) == self.process.pid, "service is not the fixture's direct child")

    def _wait(self, filename, ready, timeout):
        path = self.root / filename
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                data = read_private_json(path)
            except FileNotFoundError:
                data = None
            if data is not None and ready(data):
                return data
            require(self.process.poll() is None, f"fixture exited while waiting for {filename}")
            time.sleep(0.025)
        raise AcceptanceError(f"timed out waiting for {filename}")

    def control(self, op, args=None, timeout=15):
        self.sequence += 1
        private_json(self.root / "control.request.json", {
            "seq": self.sequence, "op": op, "args": {} if args is None else args,
        })
        result = self._wait("control.response.json", lambda value: value.get("seq") == self.sequence, timeout)
        require(result.get("ok") is True, f"fixture control operation failed: {op}")
        return result.get("data")

    def service_sample(self, after=0, timeout=5):
        return self._wait("service-sample.json", lambda value: value.get("sequence", 0) > after, timeout)

    def _kill(self):
        if self.process is None:
            return
        with suppress(ProcessLookupError):
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait(timeout=5)

    def __exit__(self, exc_type, exc, traceback):
        if exc is not None:
            self._kill()
            return False
        try:
            self.control("shutdown", timeout=55)
            require(self.process.wait(timeout=60) == 0, "fixture exited unsuccessfully")
        except BaseException:
            self._kill()
            raise
