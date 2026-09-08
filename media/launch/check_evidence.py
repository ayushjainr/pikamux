"""Offline check of the public synthetic evidence; no Pika/provider/network calls."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent
proof = root / "proof"
evidence = json.loads((proof / "evidence.json").read_text())
for name, expected in evidence["sourceSha256"].items():
    assert hashlib.sha256((proof / name).read_bytes()).hexdigest() == expected, name
result = subprocess.run([sys.executable, str(proof / "experiment.py")], check=True, capture_output=True, text=True, timeout=10)
observed = json.loads(result.stdout)
fresh, retained = observed["results"]
assert (fresh["refund_count"], retained["refund_count"]) == (2, 1)
for strategy in (fresh, retained):
    assert strategy["intended_refund_count"] == 1
    assert strategy["retry_count"] == strategy["simulated_timeout_count"] == 1
assert evidence["observed"]["freshRetryIdRefunds"] == fresh["refund_count"]
assert evidence["observed"]["originalRetryIdRefunds"] == retained["refund_count"]
subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", str(proof), "-v"], check=True, timeout=10)
print("PASS: fixture source hashes, reproducible counts and tests. Native consultation receipts remain a separately audited private capture.")
