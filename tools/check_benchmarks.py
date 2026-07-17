#!/usr/bin/env python3
import argparse, json, sys
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("report", type=Path)
p.add_argument("--classification-p95-us", type=int, default=100_000)
p.add_argument("--cache-ms", type=int, default=30_000)
a = p.parse_args()
data = json.loads(a.report.read_text(encoding="utf-8"))
errors = []
expected = {100, 500, 2000}
seen = {row["process_count"] for row in data.get("classification", [])}
if seen != expected:
    errors.append(f"matriz de procesos incompleta: {sorted(seen)}")
for row in data.get("classification", []):
    if row["p95_microseconds"] > a.classification_p95_us:
        errors.append(f"p95 clasificación {row['process_count']}: {row['p95_microseconds']} us")
for row in data.get("smart_cache", []):
    if row["elapsed_milliseconds"] > a.cache_ms:
        errors.append(f"SmartCache excede presupuesto: {row['elapsed_milliseconds']} ms")
if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("benchmark gate: PASS")
