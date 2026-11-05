"""Host resource sampling and bounded acceptance summaries on Linux."""

from collections import defaultdict
import math
import os
from pathlib import Path
import stat
import threading
import time

from .fixture import require


MIB = 1024 * 1024


def process_sample(pid):
    root = Path(f"/proc/{pid}")
    raw = (root / "stat").read_text()
    fields = raw[raw.rfind(")") + 2:].split()
    status = {}
    for line in (root / "status").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("VmRSS", "VmHWM"):
            status[key] = int(value.split()[0]) * 1024
    require(status.get("VmRSS", 0) > 0 and status.get("VmHWM", 0) > 0,
            "service process memory counters are unavailable")
    return {
        "start_ticks": int(fields[19]),
        "cpu_seconds": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
        "rss_bytes": status["VmRSS"],
        "rss_hwm_bytes": status["VmHWM"],
    }


def _disk_entries(directory):
    yield directory, directory.lstat()
    for root, dirs, files in os.walk(directory, followlinks=False):
        for name in dirs + files:
            path = Path(root, name)
            try:
                metadata = path.lstat()
            except FileNotFoundError:
                continue
            if stat.S_ISREG(metadata.st_mode) or stat.S_ISDIR(metadata.st_mode):
                yield path, metadata


def _cache_category(path):
    if path.name.endswith(("-journal", "-wal", "-shm")):
        return "sqlite_sidecar"
    if path.name == "cache.sqlite":
        return "sqlite"
    if path.parent.name == "blobs":
        return "cache_temp" if path.name.startswith(".stage-") else "blobs"
    return "cache_other"


def disk_sample(pid, cache, stage, operations=None):
    allocated = defaultdict(int)
    apparent = defaultdict(int)
    cache_charge = 0
    observed = set()

    def add(category, metadata):
        allocated[category] += metadata.st_blocks * 512
        apparent[category] += metadata.st_size
        observed.add((metadata.st_dev, metadata.st_ino))

    for path, metadata in _disk_entries(cache):
        add(_cache_category(path), metadata)
        cache_charge += max(metadata.st_size, metadata.st_blocks * 512)
    stage_files = 0
    largest_stage = 0
    for path, metadata in _disk_entries(stage):
        add("attachment_staging", metadata)
        if stat.S_ISREG(metadata.st_mode) and not path.name.endswith(".lock"):
            stage_files += 1
            largest_stage = max(largest_stage, metadata.st_size)
    if operations:
        for path in [operations, *operations.parent.glob(operations.name + "-*")]:
            try:
                add("operations_database" if path == operations else "operations_sidecar", path.lstat())
            except FileNotFoundError:
                pass
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        try:
            target = os.readlink(fd)
            if not target.endswith(" (deleted)"):
                continue
            original = Path(target[:-len(" (deleted)")])
            if not original.is_relative_to(cache) and not original.is_relative_to(stage):
                continue
            metadata = fd.stat()
        except FileNotFoundError:
            continue
        identity = (metadata.st_dev, metadata.st_ino)
        if identity not in observed and stat.S_ISREG(metadata.st_mode):
            cached = original.is_relative_to(cache)
            add("cache_pending_delete" if cached else "attachment_pending_delete", metadata)
            if cached:
                cache_charge += max(metadata.st_size, metadata.st_blocks * 512)
    return {
        "allocated_bytes": dict(sorted(allocated.items())),
        "apparent_bytes": dict(sorted(apparent.items())),
        "allocated_total_bytes": sum(allocated.values()),
        "cache_physical_charge_bytes": cache_charge,
        "stage_files": stage_files,
        "largest_stage_bytes": largest_stage,
    }


def stage_count(stage):
    return sum(
        stat.S_ISREG(metadata.st_mode) and not path.name.endswith(".lock")
        for path, metadata in _disk_entries(stage)
    )


def percentile(values, quantile):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * quantile) - 1]


class Requests:
    def __init__(self):
        self.lock = threading.Lock()
        self.values = defaultdict(list)
        self.success_times = defaultdict(list)
        self.failures = defaultdict(list)

    def record(self, account, category, elapsed):
        now = time.monotonic()
        with self.lock:
            self.values[(account, category)].append((now, elapsed))
            self.success_times[account].append(now)

    def fail(self, account, category, code):
        with self.lock:
            self.failures[(account, category, code)].append(time.monotonic())

    def report(self, started, stopped):
        with self.lock:
            groups = {}
            for (account, category), samples in sorted(self.values.items()):
                values = [elapsed for at, elapsed in samples if started <= at <= stopped]
                if not values:
                    continue
                groups.setdefault(account, {})[category] = {
                    "requests": len(values),
                    "p50_ms": round(1000 * percentile(values, 0.5), 3),
                    "p95_ms": round(1000 * percentile(values, 0.95), 3),
                    "max_ms": round(1000 * max(values), 3),
                }
            gaps = {}
            for account, times in self.success_times.items():
                between = sorted(t for t in times if started <= t <= stopped)
                points = [started, *between, stopped]
                gaps[account] = {
                    "successful_requests": len(between),
                    "max_success_gap_seconds": round(max(b - a for a, b in zip(points, points[1:])), 3),
                }
            errors = []
            for (account, category, code), times in sorted(self.failures.items()):
                count = sum(started <= at <= stopped for at in times)
                if count:
                    errors.append({"account_id": account, "operation": category, "code": code, "count": count})
        return {"latency": groups, "fairness": gaps, "errors": errors}


def queue_histogram(before, after):
    before = before["queue_wait_us"]
    after = after["queue_wait_us"]
    bounds = after["bounds"]
    require(bounds == before["bounds"] and bounds == sorted(set(bounds)) and bounds,
            "scheduler histogram changed during the run")
    result = {}
    for kind in ("foreground", "background"):
        previous = before[kind + "_counts"]
        current = after[kind + "_counts"]
        require(len(previous) == len(current) and len(current) in (len(bounds), len(bounds) + 1),
                "invalid scheduler histogram bucket count")
        counts = [end - start for start, end in zip(previous, current)]
        require(all(value >= 0 for value in counts), "scheduler histogram counter regressed")
        total = sum(counts)

        def upper(quantile):
            target = math.ceil(total * quantile)
            if target == 0:
                return None
            for index, count in enumerate(counts):
                target -= count
                if target <= 0:
                    return bounds[index] if index < len(bounds) else None
            raise AssertionError("invalid queue histogram total")

        result[kind] = {"requests": total, "p50_upper_us": upper(0.5), "p95_upper_us": upper(0.95),
                        "counts": counts}
    return {"bounds_us": bounds, "by_class": result}


def imap_delta(before, after):
    keys = ("connections", "commands", "completions", "received_bytes", "sent_bytes",
            "qresync_enables", "qresync_fetches", "condstore_fetches", "unsupported")
    result = {}
    for account, end in sorted(after.items()):
        counts = {key: end[key] - before[account][key] for key in keys}
        require(all(value >= 0 for value in counts.values()), "IMAP counter regressed")
        result[account] = {**counts, "active_final": end["active"], "max_active_observed": end["max_active"]}
    return result
