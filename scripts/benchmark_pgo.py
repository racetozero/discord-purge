"""Compare baseline and PGO binaries on offline previews that training does not use."""

from __future__ import annotations

import argparse
import gzip
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import TypedDict

from build_release import ROOT, binary_name, fixture_environment, preview, run, rust_host, write_package

MESSAGE_COUNT = 30_000
# Source, filter arguments, and expected output. These differ from the training
# data and filters, so the result does not only show the trained paths.
WORKLOADS: dict[str, tuple[str, list[str], str]] = {
    "json_substring": ("json", ["--content", "topic 3"], "Matched 4286 message(s)"),
    "csv_video": ("csv", ["--type", "video"], "Matched 10000 message(s)"),
    "zip_after_date": ("zip", ["--after-date", "2020-01-01"], f"Matched {MESSAGE_COUNT} message(s)"),
}


class WorkloadResult(TypedDict):
    name: str
    baseline_median_ms: float
    pgo_median_ms: float
    change_percent: float


class BinarySize(TypedDict):
    baseline_bytes: int
    pgo_bytes: int
    baseline_gzip_bytes: int
    pgo_gzip_bytes: int


class Report(TypedDict):
    platform: str
    rustc: str
    commit: str
    samples: int
    warmups: int
    workloads: list[WorkloadResult]
    aggregate_change_percent: float
    size: BinarySize


def benchmark(baseline: Path, optimized: Path, samples: int, warmups: int) -> Report:
    environment: dict[str, str] = dict(os.environ)
    results: list[WorkloadResult] = []
    with tempfile.TemporaryDirectory(prefix="discord-purge-benchmark-") as temporary:
        fixture = Path(temporary)
        child = fixture_environment(environment, fixture)
        rows: list[dict[str, str]] = [
            {"ID": str(1_310_000_000_000_000_000 + index * 4_194_304),
             "Contents": f"Held-out note {index} about topic {index % 7}",
             "Attachments": "https://example.invalid/clip.mp4" if index % 3 == 0 else ""}
            for index in range(MESSAGE_COUNT)
        ]
        write_package(fixture, "json", rows)
        write_package(fixture, "csv", rows)
        sources: dict[str, Path | str] = {
            "json": fixture / "json", "csv": fixture / "csv",
            "zip": shutil.make_archive(str(fixture / "package"), "zip", fixture / "json"),
        }
        binaries = (baseline, optimized)
        for name, (source, arguments, expected) in WORKLOADS.items():
            for binary in binaries:
                for _ in range(warmups):
                    preview(binary, sources[source], arguments, expected, child)
            elapsed: tuple[list[int], list[int]] = ([], [])
            for sample in range(samples):
                # Alternate the order so that neither binary always runs first.
                for choice in (0, 1) if sample % 2 == 0 else (1, 0):
                    started = time.perf_counter_ns()
                    preview(binaries[choice], sources[source], arguments, expected, child)
                    elapsed[choice].append(time.perf_counter_ns() - started)
            baseline_median, pgo_median = statistics.median(elapsed[0]), statistics.median(elapsed[1])
            results.append({
                "name": name,
                "baseline_median_ms": baseline_median / 1e6,
                "pgo_median_ms": pgo_median / 1e6,
                "change_percent": (pgo_median / baseline_median - 1) * 100,
            })
    baseline_data, pgo_data = baseline.read_bytes(), optimized.read_bytes()
    ratios = [result["pgo_median_ms"] / result["baseline_median_ms"] for result in results]
    return {
        "platform": platform.platform(),
        "rustc": run(["rustc", "--version"], environment).strip(),
        "commit": run(["git", "rev-parse", "HEAD"], environment).strip(),
        "samples": samples,
        "warmups": warmups,
        "workloads": results,
        "aggregate_change_percent": (statistics.geometric_mean(ratios) - 1) * 100,
        "size": {
            "baseline_bytes": len(baseline_data),
            "pgo_bytes": len(pgo_data),
            "baseline_gzip_bytes": len(gzip.compress(baseline_data, compresslevel=9, mtime=0)),
            "pgo_gzip_bytes": len(gzip.compress(pgo_data, compresslevel=9, mtime=0)),
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Target triple of the build; the default is the Rust host")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target" / "pgo")
    parser.add_argument("--samples", type=int, default=31)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--json", type=Path, help="Also write the results to this file")
    args = parser.parse_args()
    if args.samples < 1 or args.warmups < 0:
        parser.error(f"You passed --samples {args.samples} and --warmups {args.warmups}. Use 1 or more samples and 0 or more warm-ups.")
    target: str = args.target or rust_host(dict(os.environ))
    work: Path = args.target_dir.resolve() / target
    baseline = work / "baseline" / target / "dist" / binary_name(target)
    optimized = work / "optimized" / target / "dist" / binary_name(target)
    for binary in (baseline, optimized):
        if not binary.is_file():
            raise RuntimeError(f"Cannot find {binary}. Run `just pgo-bench`, or run scripts/build_release.py --baseline-and-pgo with the same --target and --target-dir.")
    report = benchmark(baseline, optimized, args.samples, args.warmups)
    print("workload\tbaseline_ms\tpgo_ms\tchange")
    for result in report["workloads"]:
        print(f"{result['name']}\t{result['baseline_median_ms']:.2f}\t{result['pgo_median_ms']:.2f}\t{result['change_percent']:+.2f}%")
    print(f"aggregate\t\t\t{report['aggregate_change_percent']:+.2f}%")
    size = report["size"]
    print(f"binary bytes\t{size['baseline_bytes']}\t{size['pgo_bytes']}\t{(size['pgo_bytes'] / size['baseline_bytes'] - 1) * 100:+.2f}%")
    print(f"gzip bytes\t{size['baseline_gzip_bytes']}\t{size['pgo_gzip_bytes']}\t{(size['pgo_gzip_bytes'] / size['baseline_gzip_bytes'] - 1) * 100:+.2f}%")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Benchmark failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
