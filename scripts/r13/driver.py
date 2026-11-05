"""External workload and evidence collection for the release acceptance fixture."""

import argparse
from concurrent.futures import ThreadPoolExecutor, wait
from contextlib import suppress
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import tempfile
import threading
import time

from .client import McpClient, ToolError
from .fixture import AcceptanceError, Fixture, file_sha256, private_json, require
from .metrics import MIB, Requests, disk_sample, imap_delta, process_sample, queue_histogram


MIN_DURATION = 1800
MIN_HISTORY = 10000
MIN_OVERLAP_SMOKE_DURATION = 130
MAX_DURATION = 80 * 60
ACCEPTANCE_CACHE_BYTES = 10 * 1024 * MIB
MISS_CODES = ("CACHE_MISS", "CACHE_NEEDS_VERIFICATION")
SERVICE_LIMITS = {"admitted": 20, "running": 4, "background": 1, "transfers": 4}


def wait_for(check, reason, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = check()
        if found:
            return found
        time.sleep(0.125)
    raise AcceptanceError(reason)


def new_client(ready, principal):
    client = McpClient(ready["http_base"], ready["jwt"][principal], ready["resource"])
    client.initialize()
    return client


def inbox_ref(account, uid):
    mailbox = next(folder for folder in account["mailboxes"] if folder["mailbox"] == "INBOX")
    return {"account_id": account["id"], "mailbox": "INBOX", "uid": uid,
            "uid_validity": mailbox["uid_validity"]}


class Evidence:
    def __init__(self, fixture, stream):
        self.fixture = fixture
        self.stream = stream
        self.pid = fixture.ready["service_pid"]
        self.cache = Path(fixture.ready["cache_root"])
        self.stage = Path(fixture.ready["stage_root"])
        self.operations = Path(fixture.ready["operations_db"])
        self.started = time.monotonic()
        self.initial_process = process_sample(self.pid)
        self.latest_process = self.initial_process
        self.peak_rss = 0
        self.peak_hwm = 0
        self.disk_peaks = {}
        self.peak_allocated_total = 0
        self.peak_cache_charge = 0
        self.peak_stage_files = 0
        self.peak_stage_bytes = 0
        self.scheduler_peaks = dict.fromkeys(SERVICE_LIMITS, 0)
        self.samples = 0

    def sample(self, phase, service=None, remote=None, disk=False, progress=None):
        proc = process_sample(self.pid)
        require(proc["start_ticks"] == self.initial_process["start_ticks"], "service PID was reused")
        self.latest_process = proc
        self.peak_rss = max(self.peak_rss, proc["rss_bytes"])
        self.peak_hwm = max(self.peak_hwm, proc["rss_hwm_bytes"])
        require(self.peak_hwm < 1024 * MIB, "service RSS exceeded 1 GiB")
        value = {"elapsed_seconds": round(time.monotonic() - self.started, 3),
                 "phase": phase, "process": proc}
        if service is not None:
            for key, limit in SERVICE_LIMITS.items():
                actual = service[key]
                require(0 <= actual <= limit, f"service exceeded {key} limit")
                self.scheduler_peaks[key] = max(self.scheduler_peaks[key], actual)
            value["scheduler"] = service
        if remote is not None:
            value["fixture"] = {key: remote[key] for key in ("imap", "oidc", "holds", "inserted")}
        if disk:
            measured = disk_sample(self.pid, self.cache, self.stage, self.operations)
            self.peak_allocated_total = max(self.peak_allocated_total, measured["allocated_total_bytes"])
            self.peak_cache_charge = max(self.peak_cache_charge, measured["cache_physical_charge_bytes"])
            require(self.peak_cache_charge <= self.fixture.ready["cache_max_bytes"],
                    "cache physical charge exceeded the configured global quota")
            self.peak_stage_files = max(self.peak_stage_files, measured["stage_files"])
            self.peak_stage_bytes = max(self.peak_stage_bytes, measured["largest_stage_bytes"])
            require(self.peak_stage_files <= 4, "attachment staging exceeded four files")
            require(self.peak_stage_bytes <= 32 * MIB, "attachment staging exceeded 32 MiB per file")
            for name, count in measured["allocated_bytes"].items():
                self.disk_peaks[name] = max(self.disk_peaks.get(name, 0), count)
            value["disk"] = measured
        if progress is not None:
            value["cache_status"] = progress
        self.stream.write(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.stream.flush()
        self.samples += 1
        return value

    def report(self):
        return {
            "service_pid": self.pid,
            "sample_count": self.samples,
            "service_cpu_seconds_observed": round(
                self.latest_process["cpu_seconds"] - self.initial_process["cpu_seconds"], 3),
            "service_peak_rss_bytes": self.peak_rss,
            "service_peak_rss_hwm_bytes": self.peak_hwm,
            "peak_allocated_bytes_by_category": self.disk_peaks,
            "peak_allocated_total_bytes": self.peak_allocated_total,
            "peak_cache_physical_charge_bytes": self.peak_cache_charge,
            "peak_attachment_stage_files": self.peak_stage_files,
            "peak_attachment_stage_file_bytes": self.peak_stage_bytes,
            "scheduler_peaks": self.scheduler_peaks,
        }


def cache_progress(clients, accounts):
    result = {}
    for account in accounts:
        name = account["id"]
        status, _ = clients[account["principal"]].call("get_cache_status", {"account_id": name})
        storage = status.get("storage")
        if storage:
            require(storage["used_bytes"] <= storage["max_bytes"], f"{name} exceeded its cache quota")
        result[name] = status
    return result


def capacity(fixture, evidence, clients, accounts, requests, verify_fixtures):
    cases = fixture.ready["attachments"]
    client = clients["primary"]
    slow = [case for case in cases if case["role"] == "slow"]
    extra = next(case for case in cases if case["role"] == "extra")
    require(len(slow) == 4 and all(case["bytes"] >= 8 * MIB for case in slow),
            "fixture must supply four large attachments for parked HTTP consumers")
    slow_paths = [client.attachment_path(case) for case in slow]
    fifth = client.attachment_path(extra)
    first, second = None, None
    overlapping_tail = None
    queue_sample = None
    with ThreadPoolExecutor(max_workers=5) as pool:
        fixture.control("hold_live_gets")
        futures = [pool.submit(client.park, path, case) for path, case in zip(slow_paths, slow)]
        try:
            sequence = fixture.service_sample()["sequence"]

            def live_phase():
                nonlocal sequence
                remote = fixture.control("sample")
                service = fixture.service_sample(after=sequence)
                sequence = service["sequence"]
                if (remote["holds"]["armed"] and remote["holds"]["entered"] == 4
                        and sum(account["active"] for account in remote["imap"].values()) >= 4
                        and service["running"] == 4
                        and service["transfers"] == 4 and service["background"] == 0):
                    return service, remote
                return None

            try:
                service, remote = wait_for(live_phase, "four GETs did not occupy actual IMAP and transfer slots")
                client.expect_busy(fifth)
                first = evidence.sample("four_live_imap_gets", service, remote, disk=True)
                probe = new_client(fixture.ready, "primary")
                queue_request = pool.submit(probe.call, "list_emails",
                    {"account_id": accounts[0]["id"], "mailbox": "INBOX", "limit": 1})

                def queued_while_held():
                    nonlocal sequence
                    fresh = fixture.service_sample(after=sequence)
                    sequence = fresh["sequence"]
                    require(not queue_request.done(), "foreground request escaped four held IMAP slots")
                    return fresh if fresh["running"] == 4 and fresh["admitted"] > fresh["running"] else None

                queue_sample = wait_for(queued_while_held, "fifth IMAP operation did not wait in the backend queue",
                                        timeout=5)
                evidence.sample("foreground_queued_behind_four_live_gets", queue_sample)
                if verify_fixtures:
                    finished, _ = wait([queue_request], timeout=15)
                    require(bool(finished), "backend queue did not enforce its waiting limit")
            finally:
                fixture.control("release_live_gets")
            try:
                _, queue_elapsed = queue_request.result(timeout=35)
            except ToolError as error:
                if error.code != "BUSY":
                    raise
                queue_outcome = "BUSY"
                queue_elapsed = error.elapsed
                requests.fail(accounts[0]["id"], "live.list_during_backend_saturation", error.code)
            else:
                queue_outcome = "ok"
                requests.record(accounts[0]["id"], "live.list_after_backend_queue", queue_elapsed)
            if verify_fixtures:
                require(queue_outcome == "BUSY", "held backend queue did not return BUSY")
            for future in futures:
                future.result(timeout=50)

            def parked_phase():
                nonlocal sequence
                service = fixture.service_sample(after=sequence)
                sequence = service["sequence"]
                if service["transfers"] == 4 and service["running"] < 4:
                    measured = disk_sample(evidence.pid, evidence.cache, evidence.stage, evidence.operations)
                    if measured["stage_files"] == 4:
                        return service
                return None

            service = wait_for(parked_phase, "four HTTP body consumers did not retain their transfer leases")
            client.expect_busy(fifth)
            for account in accounts:
                name = account["id"]
                _, elapsed = clients[account["principal"]].call(
                    "list_emails", {"account_id": name, "mailbox": "INBOX", "limit": 1})
                requests.record(name, "live.list_while_http_saturated", elapsed)
            if not verify_fixtures:
                inserted = fixture.control("append_tail")

                def advanced_during_consumption():
                    nonlocal sequence
                    progress = cache_progress(clients, accounts)
                    observed = fixture.service_sample(after=sequence)
                    sequence = observed["sequence"]
                    require(observed["transfers"] == 4, "an HTTP lease ended before concurrent background proof")
                    coverage = coverage_report(progress, accounts, inserted)
                    if all(coverage[row["account_id"]][row["mailbox"]]["tail_advanced"] for row in inserted):
                        return inserted
                    return None

                overlapping_tail = wait_for(advanced_during_consumption,
                    "background did not confirm both account tails while four HTTP leases were held", timeout=60)
            service = fixture.service_sample(after=sequence)
            require(service["transfers"] == 4, "HTTP transfer leases ended before the final saturation sample")
            second = evidence.sample("four_parked_http_consumers", service, fixture.control("sample"), disk=True)
        finally:
            for future in futures:
                with suppress(Exception):
                    future.result(timeout=50).close()

    def cleaned():
        nonlocal sequence
        service = fixture.service_sample(after=sequence)
        sequence = service["sequence"]
        disk = disk_sample(evidence.pid, evidence.cache, evidence.stage, evidence.operations)
        return service["transfers"] == 0 and disk["stage_files"] == 0

    wait_for(cleaned, "cancelling four parked consumers did not release all staging and transfer leases")
    verified = []
    selected = [extra]
    if verify_fixtures:
        selected.append(slow[0])
        selected.extend(case for case in cases if case["role"] in ("boundary", "multilayer"))
    for case in selected:
        path = client.attachment_path(case)
        elapsed = client.download(path, case, lambda: evidence.sample(
            "verified_attachment_" + case["role"], fixture.service_sample(), disk=True))
        requests.record(case["account_id"], "live.attachment." + case["role"], elapsed)
        verified.append({"role": case["role"], "bytes": case["bytes"], "sha256": case["sha256"]})
    if verify_fixtures:
        require(any(case["role"] == "boundary" and case["bytes"] == 32 * MIB for case in verified),
                "missing verified 32 MiB attachment")
        require(any(case["role"] == "multilayer" for case in verified), "missing verified multilayer attachment")
    return {"imap": {"running": first["scheduler"]["running"], "transfers": 4, "fifth_http_status": 429},
            "http": {"running": second["scheduler"]["running"], "transfers": 4, "fifth_http_status": 429},
            "backend_queue": {"admitted": queue_sample["admitted"], "running": queue_sample["running"],
                              "queued_request_outcome": queue_outcome,
                              "queued_request_elapsed_ms": round(1000 * queue_elapsed, 3),
                              "queued_request_succeeded_after_release": queue_outcome == "ok"},
            "foreground_accounts_during_http_saturation": [account["id"] for account in accounts],
            "background_tail_during_http_saturation": overlapping_tail,
            "cancelled_consumers": 4, "staging_clean_after_cancel": True, "digests_verified": verified}


def prepare_warm(clients, accounts, requests):
    identities = {}
    for account in accounts:
        name = account["id"]
        client = clients[account["principal"]]
        ref = inbox_ref(account, account["hot_uid"])
        live, elapsed = client.call("get_email", ref)
        requests.record(name, "live.get_warmup", elapsed)
        subject = live.get("subject", "")
        require(subject.count("长中文标题") >= 40 and "续行边界" in subject,
                f"{name} did not return the folded long Chinese fixture subject")
        identities[name] = (subject, live.get("body"))

        def warmed():
            try:
                local, elapsed = client.call("get_email", {**ref, "read_source": "cache"})
            except ToolError as error:
                if error.code not in MISS_CODES:
                    raise
                requests.fail(name, "cache.get_warmup", error.code)
                return False
            require(local.get("read_source") == "cache" and isinstance(local.get("body"), str)
                    and local["body"] == live.get("body") and local.get("subject") == subject,
                    "cached warm subject or body differs from the live response")
            requests.record(name, "cache.get_warmup", elapsed)
            return True

        wait_for(warmed, "warm body did not become readable from the persistent cache")
        for uid in account.get("cold_uids", range(account["cold_uid"], account["cold_uid"] + 8)):
            cold_ref = inbox_ref(account, uid)
            try:
                client.call("get_email", {**cold_ref, "read_source": "cache"})
            except ToolError as error:
                if error.code not in MISS_CODES:
                    raise
                requests.fail(name, "cache.get_cold_probe", error.code)
                category = "live.get_cold_uid" if error.code == "CACHE_MISS" else "live.get_unverified_uid"
            else:
                category = "live.get_precached_uid"
            _, elapsed = client.call("get_email", cold_ref)
            requests.record(name, category, elapsed)
    require(len(set(identities.values())) == len(accounts), "same-UID warm reads crossed account identities")


def frontend(ready, account, requests, stop, capacity_active, interval):
    client = new_client(ready, account["principal"])
    name = account["id"]
    ref = inbox_ref(account, account["hot_uid"])
    plan = [
        ("get_email", ref, "live.get_hot_uid"),
        ("get_email", {**ref, "read_source": "cache"}, "cache.get_hot_uid"),
        ("list_emails", {"account_id": name, "mailbox": "INBOX", "limit": 8}, "live.list"),
        ("list_emails", {"account_id": name, "mailbox": "INBOX", "limit": 8, "read_source": "cache"}, "cache.list"),
    ]
    index = 0
    while not stop.is_set():
        tool, args, category = plan[index % len(plan)]
        try:
            result, elapsed = client.call(tool, args)
        except ToolError as error:
            requests.fail(name, category, error.code)
            expected_saturation = error.code == "BUSY" and capacity_active.is_set()
            expected_cache_miss = error.code in MISS_CODES and category.startswith("cache.")
            if not (expected_saturation or expected_cache_miss):
                raise
        else:
            if category.startswith("cache."):
                require(result.get("read_source") == "cache", "cache workload response lost its source")
            requests.record(name, category, elapsed)
        index += 1
        stop.wait(interval)


def initial_materialized(coverage, folder):
    expected = folder["message_count"]
    if (coverage.get("high_uid") != folder["last_uid"]
            or not coverage.get("existence_flags_verified")
            or any(coverage.get(field) != expected for field in (
                "known_messages", "complete_listing_facts", "complete_headers", "indexed_headers",
                "exact_thread_index", "verified_manifests", "indexed_bodies",
            ))
            or not all(coverage.get(field) for field in (
                "listing_facts_complete", "headers_complete", "exact_thread_index_complete",
                "manifests_complete", "body_index_complete",
            ))):
        return False
    expected_attachments = folder.get("expected_attachments")
    if not isinstance(expected_attachments, int) or expected_attachments < 0:
        return False
    work = coverage.get("work") or {}
    if coverage.get("gaps") != [] or any(work.get(state) != 0 for state in ("pending", "retrying", "quota_blocked", "terminal")):
        return False
    for name, count in (("raw", expected), ("full_body", expected), ("attachments", expected_attachments)):
        layer = coverage.get(name) or {}
        if (layer.get("materialization_complete") is not True or layer.get("materialized") != count
                or any(layer.get(state) != 0 for state in ("derivable_from_raw", "missing", "omitted", "unsupported"))):
            return False
    return True


def coverage_report(progress, accounts, inserted):
    result = {}
    pending = {(entry["account_id"], entry["mailbox"]): entry for entry in inserted}
    for account in accounts:
        name = account["id"]
        status = progress.get(name) or {}
        sync = status.get("sync") or {}
        by_mailbox = {row["mailbox"]: row["progress"] for row in sync.get("folders", [])}
        scope_counts = {}
        for folder in account["mailboxes"]:
            mailbox = folder["mailbox"]
            scope = by_mailbox.get(mailbox) or {}
            history = scope.get("history") or {}
            coverage = scope.get("history_coverage") or {}
            tail = scope.get("tail") or {}
            append = pending.get((name, mailbox))
            scope_counts[mailbox] = {
                "initial_messages": folder["message_count"],
                "initial_expected_attachments": folder.get("expected_attachments"),
                "discovered_initial_messages": coverage.get("known_messages", 0),
                "initial_scope_complete": bool(history.get("complete") and coverage.get("discovery_complete")
                    and history["high_uid"] == folder["last_uid"] and scope.get("uid_validity") == folder["uid_validity"]),
                "initial_materialization_complete": initial_materialized(coverage, folder),
                "initial_coverage": coverage or None,
                "tail_advanced": append is not None and bool(tail.get("complete")
                    and tail["high_uid"] >= append["uid"] and scope.get("uid_validity") == append["uid_validity"]),
                "coverage": scope.get("current_coverage"),
            }
        result[name] = scope_counts
    return result


def materialization_complete(progress, accounts):
    return all(folder["initial_scope_complete"] and folder["initial_materialization_complete"]
               for folders in coverage_report(progress, accounts, []).values() for folder in folders.values())


def workload(fixture, evidence, clients, accounts, requests, args):
    baseline_remote = fixture.control("sample")
    baseline_service = fixture.service_sample()
    baseline = process_sample(evidence.pid)
    start = time.monotonic()
    minimum_deadline = start + args.duration_seconds
    maximum_deadline = minimum_deadline if args.smoke else start + MAX_DURATION
    append_at = start + min(5, args.duration_seconds / 3)
    capacity_at = start + args.duration_seconds / 2 if args.duration_seconds >= MIN_OVERLAP_SMOKE_DURATION else float("inf")
    append_waves = []
    capacity_waves = []
    next_remote, next_disk, next_progress = start, start, start
    stop = threading.Event()
    capacity_active = threading.Event()
    verified_progress = None
    verified_at = None
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(frontend, fixture.ready, account, requests, stop, capacity_active,
                               args.interval_seconds) for account in accounts]
        try:
            while time.monotonic() < maximum_deadline:
                for future in futures:
                    if future.done():
                        future.result()
                now = time.monotonic()
                if now >= capacity_at and minimum_deadline - now > 60:
                    capacity_active.set()
                    try:
                        capacity_waves.append(capacity(fixture, evidence, clients, accounts, requests, False))
                    finally:
                        capacity_active.clear()
                    capacity_at = float("inf")
                if now >= append_at:
                    inserted = fixture.control("append_tail")
                    append_waves.append({"elapsed_seconds": round(now - start, 3), "inserted": inserted})
                    append_at = now + 300 if minimum_deadline - now > 360 else float("inf")
                service = fixture.service_sample()
                sampled = None
                observed = None
                if now >= next_remote:
                    sampled = fixture.control("sample")
                    next_remote = now + 5
                if now >= next_progress:
                    observed = cache_progress(clients, accounts)
                    observed_at = time.monotonic()
                    next_progress = now + 15
                    if (minimum_deadline <= observed_at <= maximum_deadline
                            and materialization_complete(observed, accounts)):
                        verified_progress = observed
                        verified_at = observed_at
                disk = now >= next_disk
                if disk:
                    next_disk = now + 20
                evidence.sample("sustained", service, sampled, disk, observed)
                if verified_progress is not None:
                    break
                stop.wait(max(0, min(1, maximum_deadline - time.monotonic())))
        finally:
            end = min(time.monotonic(), maximum_deadline)
            stop.set()
        for future in futures:
            future.result()
    final_remote = fixture.control("sample")
    final_service = fixture.service_sample()
    progress = cache_progress(clients, accounts)
    final = evidence.sample("sustained_final", final_service, final_remote, disk=True, progress=progress)
    elapsed = end - start
    cpu_seconds = final["process"]["cpu_seconds"] - baseline["cpu_seconds"]
    return {
        "duration_seconds": round(elapsed, 3),
        "minimum_duration_seconds": args.duration_seconds,
        "materialization_extension_seconds": round(max(0, elapsed - args.duration_seconds), 3),
        "initial_materialization_observed_at_seconds": None if verified_at is None else round(verified_at - start, 3),
        "requests": requests.report(start, end),
        "queue_wait": queue_histogram(baseline_service, final_service),
        "imap_plaintext": imap_delta(baseline_remote["imap"], final_remote["imap"]),
        "oidc": {"initial_jwks_requests": baseline_remote["oidc"]["jwks"],
                 "final_jwks_requests": final_remote["oidc"]["jwks"]},
        "background": coverage_report(verified_progress or progress, accounts, final_remote["inserted"]),
        "background_at_final_sample": coverage_report(progress, accounts, final_remote["inserted"]),
        "append_waves": append_waves,
        "attachment_capacity_waves": capacity_waves,
        "service_cpu_seconds": round(cpu_seconds, 3),
        "service_average_percent_of_one_cpu": round(100 * cpu_seconds / elapsed, 3),
    }


def verify_workload(work, accounts, args):
    require(set(work["requests"]["fairness"]) == {account["id"] for account in accounts},
            "foreground did not succeed for both accounts")
    for account in accounts:
        name = account["id"]
        groups = work["requests"]["latency"].get(name, {})
        for operation in ("live.get_hot_uid", "live.list", "cache.get_hot_uid", "cache.list"):
            require(groups.get(operation, {}).get("requests", 0) > 0, f"{name} had no successful {operation}")
        require(work["imap_plaintext"][name]["commands"] > 0, "account had no measured IMAP activity")
    if args.duration_seconds >= MIN_OVERLAP_SMOKE_DURATION:
        require(len(work["attachment_capacity_waves"]) == 1, "missing capacity check during sustained load")
    if args.smoke:
        return
    require(work["duration_seconds"] >= MIN_DURATION, "sustained workload lasted under 1,800 seconds")
    require(work["initial_materialization_observed_at_seconds"] is not None,
            "initial full materialization did not settle within the sustained workload limit")
    refreshes = work["oidc"]["final_jwks_requests"] - work["oidc"]["initial_jwks_requests"]
    require(refreshes >= MIN_DURATION // 300 - 1, "sustained authentication missed repeated JWKS refresh")
    require(work["queue_wait"]["by_class"]["background"]["requests"] > 0, "no background queue admissions")
    require(len(work["append_waves"]) >= 5, "missing periodic background tail changes")
    known = 0
    for account in accounts:
        name = account["id"]
        folders = work["background"][name]
        require(folders and all(folder["initial_scope_complete"] for folder in folders.values()),
                f"{name} did not finish initial history discovery")
        require(all(folder["initial_materialization_complete"] for folder in folders.values()),
                f"{name} did not finish initial raw, body, index and attachment materialization")
        require(any(folder["tail_advanced"] for folder in folders.values()), f"{name} did not advance its live tail")
        known += sum(folder["discovered_initial_messages"] for folder in folders.values())
        require(work["imap_plaintext"][name]["qresync_fetches"] > 0,
                f"{name} did not complete a bounded QRESYNC FETCH")
        require(work["requests"]["fairness"][name]["max_success_gap_seconds"] <= 60,
                f"{name} had a foreground success gap over 60 seconds")
    require(known >= MIN_HISTORY, "background discovery observed fewer than 10,000 initial messages")


def exercise_cases(fixture, clients, requests, smoke):
    cases = fixture.ready["cases"]
    client = clients["primary"]
    search = {**cases["search"], "search_source": "cache"}
    if smoke:
        account = next(account for account in fixture.ready["accounts"] if account["id"] == search["account_id"])
        search["before_uid"] = account["hot_uid"] + 1

    def search_body():
        args = search
        for _ in range(32):
            try:
                page, elapsed = client.call("search_emails", args)
            except ToolError as error:
                requests.fail(search["account_id"], "cache.search_long_chinese", error.code)
                if error.code in ("CACHE_NEEDS_VERIFICATION", "CURSOR_UNAVAILABLE"):
                    return None
                raise
            requests.record(search["account_id"], "cache.search_long_chinese", elapsed)
            hits = [message for message in page["emails"] if message.get("cache_match", {}).get("body")]
            if hits:
                return len(hits)
            if not page.get("next_cache_cursor"):
                return None
            args = {key: value for key, value in search.items() if key != "before_uid"}
            args["cache_cursor"] = page["next_cache_cursor"]
        return None

    found = wait_for(search_body, "long Chinese cache search returned no body index hit")

    seed = cases["conversation_seed"]
    expected = set(seed["mailboxes"])
    if smoke:
        for mailbox in seed["mailboxes"]:
            client.call("list_emails", {"account_id": seed["account_id"], "mailbox": mailbox,
                                      "before_uid": 6, "limit": 5})
        client.call("get_cache_status", {"account_id": seed["account_id"]})

    def conversation():
        locations = set()
        args = {**seed, "read_source": "cache", "max_body_length": 0}
        for _ in range(60):
            try:
                page, elapsed = client.call("read_conversation", args)
            except ToolError as error:
                requests.fail(seed["account_id"], "cache.cross_mailbox_conversation", error.code)
                if error.code in ("CACHE_NEEDS_VERIFICATION", "CURSOR_UNAVAILABLE"):
                    return None
                raise
            requests.record(seed["account_id"], "cache.cross_mailbox_conversation", elapsed)
            locations.update(loc["mailbox"] for message in page["messages"] for variant in message["variants"]
                             for loc in variant["locations"] if loc["uid"] == seed["uid"])
            if locations == expected or page["cursor"] is None:
                break
            args = {"account_id": seed["account_id"], "cursor": page["cursor"], "read_source": "cache"}
        return locations if locations == expected else None

    locations = wait_for(conversation, "cached conversation did not find the fixture copy in both folders")
    return {"long_chinese_body_index_hits": found, "conversation_mailboxes": sorted(locations)}


def run(args, report, stream):
    with tempfile.TemporaryDirectory(prefix="r13-acceptance-") as temp:
        root = Path(temp) / "fixture"
        root.mkdir(mode=0o700)
        with Fixture(args.harness, root, args.fixture_log) as fixture:
            ready = fixture.ready
            report["processes"] = {"driver_pid": os.getpid(), "fixture_host_pid": fixture.process.pid,
                                   "service_pid": ready["service_pid"]}
            accounts = ready["accounts"]
            require(len(accounts) == 2 and len({a["id"] for a in accounts}) == 2, "expected two fixture accounts")
            require(sum(folder["message_count"] for account in accounts for folder in account["mailboxes"])
                    >= MIN_HISTORY, "fixture does not contain 10,000 initial scope messages")
            if not args.smoke:
                require(ready["cache_max_bytes"] == ACCEPTANCE_CACHE_BYTES,
                        "acceptance fixture does not have the 10 GiB global cache quota")
                require(all(isinstance(folder.get("expected_attachments"), int)
                    for account in accounts for folder in account["mailboxes"]),
                    "acceptance fixture did not declare initial attachment counts")
            report["cache_global_quota_bytes"] = ready["cache_max_bytes"]
            report["dataset"] = [{"id": a["id"], "mailboxes": a["mailboxes"]} for a in accounts]
            clients = {principal: new_client(ready, principal) for principal in ("primary", "secondary")}
            try:
                clients["secondary"].call("list_emails", {"account_id": accounts[0]["id"], "limit": 1})
            except ToolError as error:
                require(error.code == "ACCOUNT_UNAVAILABLE", "secondary principal did not receive the expected ACL error")
            else:
                raise AcceptanceError("secondary principal reached the first account")
            report["cross_account_acl"] = "ACCOUNT_UNAVAILABLE"
            requests = Requests()
            evidence = Evidence(fixture, stream)
            try:
                prepare_warm(clients, accounts, requests)
                report["attachment_capacity"] = capacity(fixture, evidence, clients, accounts, requests, True)
                report["sustained"] = workload(fixture, evidence, clients, accounts, requests, args)
                verify_workload(report["sustained"], accounts, args)
                report["fixture_cases"] = exercise_cases(fixture, clients, requests, args.smoke)
            finally:
                report["resources"] = evidence.report()
                report["requests"] = requests.report(evidence.started, time.monotonic())
            for account in accounts:
                measured = report["requests"]["latency"][account["id"]]
                require(measured.get("live.get_cold_uid", {}).get("requests", 0) > 0,
                        f"{account['id']} had no live sample following an observed cache miss")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harness", required=True, type=Path, help="release ignored-test executable")
    parser.add_argument("--output", required=True, type=Path, help="write private JSON summary here")
    parser.add_argument("--samples", type=Path, help="private NDJSON sample path (default: OUTPUT.samples.ndjson)")
    parser.add_argument("--fixture-log", type=Path, help="optional private release fixture log")
    parser.add_argument("--duration-seconds", type=int, default=MIN_DURATION)
    parser.add_argument("--interval-seconds", type=float, default=0.45, help="per-account foreground pacing")
    parser.add_argument("--smoke", action="store_true",
                        help="run plumbing (130 seconds also exercises overlap) without claiming R13 acceptance")
    args = parser.parse_args()
    minimum_duration = 10 if args.smoke else MIN_DURATION
    maximum_duration = MAX_DURATION if args.smoke else MAX_DURATION - 30
    if not minimum_duration <= args.duration_seconds <= maximum_duration:
        parser.error(f"duration must be between {minimum_duration} and {maximum_duration} seconds")
    if not 0.15 <= args.interval_seconds <= 3:
        parser.error("--interval-seconds must be between 0.15 and 3")
    if sys.platform != "linux":
        parser.error("the acceptance driver requires Linux")
    args.harness = args.harness.resolve(strict=True)
    args.output = args.output.resolve()
    args.samples = (args.samples or args.output.with_suffix(".samples.ndjson")).resolve()
    paths = [args.samples, args.output]
    if args.fixture_log:
        args.fixture_log = args.fixture_log.resolve()
        paths.append(args.fixture_log)
    if len(set(paths)) != len(paths) or any(path.exists() for path in paths):
        parser.error("output, sample and fixture log paths must be distinct new files")
    namespace = os.readlink("/proc/self/ns/net")
    if os.environ.get("R13_PARENT_NETNS") is None:
        env = {**os.environ, "R13_PARENT_NETNS": namespace}
        return subprocess.run(["unshare", "--user", "--map-root-user", "--net", "--",
                               sys.executable, "-B", *sys.argv], env=env, check=False).returncode
    report = {"schema": 1, "mode": "smoke" if args.smoke else "acceptance", "passed": False,
              "harness_sha256": file_sha256(args.harness), "kernel": platform.release(),
              "architecture": platform.machine(), "duration_requested_seconds": args.duration_seconds,
              "started_at_utc": datetime.now(timezone.utc).isoformat()}
    driver_dir = Path(__file__).resolve().parent
    driver_sources = [Path(sys.argv[0]).resolve(), *sorted(driver_dir.glob("*.py"))]
    report["driver_sha256"] = {
        source.relative_to(driver_dir.parent).as_posix(): file_sha256(source) for source in driver_sources
    }
    try:
        require(namespace != os.environ["R13_PARENT_NETNS"], "network namespace isolation failed")
        subprocess.run(["ip", "link", "set", "lo", "up"], check=True, capture_output=True)
        require([name for _, name in socket.if_nameindex()] == ["lo"], "fixture network has non-loopback links")
        report["network"] = "isolated_linux_loopback"
        with os.fdopen(os.open(args.samples, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            run(args, report, stream)
        report["passed"] = True
    except AcceptanceError as error:
        report["error"] = str(error)
    except (OSError, ValueError, KeyError, StopIteration, TimeoutError, subprocess.SubprocessError) as error:
        report["error"] = type(error).__name__
    finally:
        report["finished_at_utc"] = datetime.now(timezone.utc).isoformat()
        if args.samples.exists():
            report["samples_sha256"] = file_sha256(args.samples)
        if args.fixture_log and args.fixture_log.exists():
            report["fixture_log_sha256"] = file_sha256(args.fixture_log)
        private_json(args.output, report)
    print(f"R13 {report['mode']}: {'passed' if report['passed'] else report['error']}")
    return 0 if report["passed"] else 1
