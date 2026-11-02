"""Regenerate Rust's deterministic R00 attachment fixtures with uv."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / "scripts/attachment-delivery-probe/probe.py"
DESTINATION = Path(__file__).resolve().parent


def main():
    spec = importlib.util.spec_from_file_location("attachment_probe", SOURCE)
    probe = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probe)
    evidence = json.loads((SOURCE.parent / "evidence/selftest-2026-11-02.json").read_text(encoding="utf-8"))
    expected = {fixture["fixture_id"]: fixture for fixture in evidence["fixtures"]}
    fixtures = []
    for key, fixture in probe.fixtures().items():
        original = expected[key]
        assert fixture["sha256"] == original["sha256"]
        assert fixture["decoded_size"] == original["decoded_size"]
        extension = Path(fixture["filename"]).suffix
        path = f"{key}{extension}"
        (DESTINATION / path).write_bytes(fixture["bytes"])
        parsed = probe.parse_fixture(key, fixture["bytes"])
        assert parsed == original["parsed"]
        fixtures.append({**probe.public_fixture(fixture), "path": path, "parsed": parsed})
    (DESTINATION / "manifest.json").write_text(json.dumps({"source":"scripts/attachment-delivery-probe/probe.py::fixtures", "fixtures":fixtures},ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    print(f"Verified {len(fixtures)} fixtures against R00 SHA-256 and parser results.")


if __name__ == "__main__":
    main()
