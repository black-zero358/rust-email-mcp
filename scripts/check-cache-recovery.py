"""Run the release cache recovery smoke test against synthetic data on Linux.

uv run --no-project python scripts/check-cache-recovery.py \
    --binary /path/to/rust-email-mcp --schema-version 6
"""

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
from uuid import UUID


MIB = 1024 * 1024
SENTINEL = b"SYNTHETIC_INVALID_SQLITE_CACHE_RECOVERY_SENTINEL"


class SmokeError(Exception):
    """A release recovery invariant failed."""


def require(condition, message):
    if not condition:
        raise SmokeError(message)


def private_file(path, data):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as file:
        file.write(data)


def mode(path):
    return stat.S_IMODE(path.lstat().st_mode)


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(MIB), b""):
            digest.update(chunk)
    return digest.hexdigest()


def configuration(root):
    config = root / "cache.json"
    private_file(config, json.dumps({
        "version": 1,
        "mode": "full",
        "directory": "cache",
        "max_bytes": 32 * MIB,
        "accounts": [{"account_id": "default", "mailboxes": ["INBOX"], "max_bytes": 16 * MIB}],
    }).encode())
    return {
        "EMAIL_ADDRESS": "owner@example.invalid",
        "EMAIL_PASSWORD": "synthetic-password",
        "IMAP_HOST": "imap.example.invalid",
        "IMAP_PORT": "993",
        "SMTP_HOST": "smtp.example.invalid",
        "SMTP_PORT": "465",
        "SMTP_STARTTLS": "false",
        "DRAFTS_MAILBOX": "Drafts",
        "TRASH_MAILBOX": "Trash",
        "MAIL_MODE": "read",
        "MCP_RESOURCE": "https://mail.example.invalid/mcp",
        "OIDC_ISSUER": "https://auth.example.invalid/",
        "OIDC_AUDIENCE": "https://mail.example.invalid/mcp",
        "OIDC_SUBJECTS": "synthetic-subject",
        "OIDC_SCOPE": "email:mcp",
        "BIND": "127.0.0.1:18590",
        "DATABASE_PATH": str(root / "operations.sqlite"),
        "ACCOUNT_REGISTRY_PATH": str(root / "accounts.sqlite"),
        "CACHE_CONFIG_PATH": str(config),
        "ATTACHMENT_STAGING_DIR": str(root / "downloads"),
    }


def witness_identity(witness, registry):
    record = witness.read_bytes()
    require(len(record) == 106 and record[:10] == b"REMCPRUN\x00\x01", "invalid witness format")
    require(hashlib.sha256(record[:74]).digest() == record[74:], "invalid witness checksum")
    binding = hashlib.sha256(b"rust-email-mcp/cache-run-registry/v1\0" + os.fsencode(registry)).digest()
    require(record[10:42] == binding, "witness bound to a different registry")
    require(mode(witness) == 0o600, "invalid witness permissions")
    instance = UUID(bytes=record[42:58])
    run = UUID(bytes=record[58:74])
    require(instance.version == run.version == 4, "invalid witness UUIDs")
    return instance, run


def smoke(binary, schema_version, unshare):
    binary_hash = file_hash(binary)
    with tempfile.TemporaryDirectory(prefix="mail-cache-recovery-") as directory:
        root = Path(directory).resolve()
        journal = root / "operations.sqlite"
        registry = root / "accounts.sqlite"
        witness = root / "accounts.sqlite.cache-run"
        cache = root / "cache"
        manifest = cache / "manifest.json"
        private_file(journal, SENTINEL)
        env = configuration(root)

        def invoke(*, off=False):
            command = [unshare, "--user", "--map-root-user", "--net", "--", str(binary)]
            settings = env.copy()
            if off:
                del settings["CACHE_CONFIG_PATH"]
            else:
                command.append("--cache-restored")
            result = subprocess.run(command, cwd=root, env=settings, capture_output=True, timeout=20)
            require(journal.read_bytes() == SENTINEL, "operation journal bytes changed")
            require(not any(root.glob("operations.sqlite-*")), "operation journal sidecar created")
            require(not (root / "downloads").exists(), "attachment staging opened")
            return result

        def restore(epoch):
            result = invoke()
            require(result.returncode == 0, f"restore {epoch} failed: {result.stderr.decode(errors='replace').strip()}")
            state = json.loads(manifest.read_bytes())
            require(state["restore_epoch"] == epoch and state["clean"] is False, f"incorrect recovery receipt for epoch {epoch}")
            require(mode(root / "accounts.sqlite.cache-run.lock") == 0o600, "invalid witness lock permissions")
            require(not (root / "accounts.sqlite.cache-run.stage").exists(), "witness staging left behind")
            return witness_identity(witness, registry)

        instance, first = restore(1)
        databases = list(cache.glob("*/cache.sqlite"))
        require(len(databases) == 1, "expected one cache account partition")
        database = databases[0]
        with closing(sqlite3.connect(database)) as db, db:
            actual_schema = db.execute("PRAGMA user_version").fetchone()[0]
            require(actual_schema == schema_version, f"expected schema {schema_version}, found {actual_schema}")
            db.execute("INSERT INTO mailboxes(mailbox,uid_validity,high_uid,checked_at_ms) VALUES('INBOX',9,7,100)")

        second_instance, second = restore(2)
        require(second_instance == instance and second != first, "restore replaced the instance or reused its run UUID")
        with closing(sqlite3.connect(database)) as db:
            require(db.execute("SELECT dirty FROM mailboxes WHERE mailbox='INBOX'").fetchone() == (1,), "restored mailbox is not dirty")
            require(db.execute("SELECT restore_epoch FROM cache_owner").fetchone() == (2,), "partition recovery epoch is incorrect")

        before_off = manifest.read_bytes()
        off = invoke(off=True)
        require(off.returncode != 0 and b"STORAGE_UNAVAILABLE" in off.stderr, "cache-off start did not reach the invalid journal")
        off_instance, off_run = witness_identity(witness, registry)
        require(off_instance == instance and off_run not in (first, second), "failed cache-off start did not advance the witness")
        require(manifest.read_bytes() == before_off, "cache-off start changed the cache manifest")

        last_instance, last = restore(3)
        require(last_instance == instance and last not in (first, second, off_run), "restore after cache-off reused the witness")
        before_corruption = manifest.read_bytes()
        corrupt = b"CORRUPT_SYNTHETIC_WITNESS"
        witness.write_bytes(corrupt)
        failure = invoke()
        require(failure.returncode != 0 and b"Cache run witness is unavailable or invalid" in failure.stderr, "corrupt witness was not rejected before cache recovery")
        require(witness.read_bytes() == corrupt, "failed recovery modified the corrupt witness")
        require(manifest.read_bytes() == before_corruption, "failed recovery changed the cache manifest")
        for path in [cache, *cache.rglob("*")]:
            require(not path.is_symlink() and mode(path) == (0o700 if path.is_dir() else 0o600), f"invalid cache permissions for {path.relative_to(root)}")

    require(file_hash(binary) == binary_hash, "release binary changed while the smoke test was running")
    return {
        "release_sha256": binary_hash,
        "schema": schema_version,
        "restore_epochs": [1, 2, 3],
        "witness_rotates_on_off_failed_start": True,
        "corrupt_witness_rejected_before_journal": True,
        "network_namespace": "isolated",
        "journal_sentinel": "unchanged",
        "cache_modes": "0700/0600",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="path to the Linux release binary")
    parser.add_argument("--schema-version", required=True, type=int, help="expected SQLite user_version")
    args = parser.parse_args()
    if args.schema_version < 1:
        parser.error("--schema-version must be positive")
    unshare = shutil.which("unshare") if sys.platform == "linux" else None
    if unshare is None:
        parser.error("Linux util-linux unshare and network namespaces are required")
    try:
        binary = args.binary.resolve(strict=True)
        require(binary.is_file() and os.access(binary, os.X_OK), "--binary must name an executable file")
        result = smoke(binary, args.schema_version, unshare)
    except (OSError, SmokeError, ValueError, sqlite3.Error, subprocess.TimeoutExpired) as error:
        parser.exit(1, f"cache recovery smoke test failed: {error}\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
