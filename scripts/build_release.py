"""Build a native release with offline PGO training and verified archives."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP = "discord-purge"
TARGETS = (
    "aarch64-apple-darwin", "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl",
    "aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc",
)


def binary_name(target: str) -> str:
    return APP + (".exe" if "windows" in target else "")


def run(command: list[str], environment: dict[str, str]) -> str:
    result = subprocess.run(
        command, cwd=ROOT, env=environment, check=True,
        text=True, stdout=subprocess.PIPE, stderr=sys.stderr,
    )
    return result.stdout


def rust_host(environment: dict[str, str]) -> str:
    return next(line.removeprefix("host: ") for line in run(
        ["rustc", "-vV"], environment).splitlines() if line.startswith("host: "))


def fixture_environment(environment: dict[str, str], fixture: Path) -> dict[str, str]:
    """Remove credentials and keep all app state inside the fixture."""
    child = {
        key: value for key, value in environment.items()
        if not key.startswith("DISCORD_")
        and not any(word in key.upper() for word in ("TOKEN", "PASSWORD", "SECRET"))
    }
    child.update({
        "DISCORD_PURGE_STATE_DIR": str(fixture / "state"),
        "DISCORD_PURGE_IDENTITY_DIR": str(fixture / "identity"),
    })
    return child


def write_package(root: Path, extension: str, rows: list[dict[str, str]]) -> None:
    """Write an invented one-channel data package to root/extension."""
    channel = root / extension / "messages" / "c111"
    channel.mkdir(parents=True)
    (channel / "channel.json").write_text(
        json.dumps({"id": "111", "name": "training", "type": 0,
                    "guild": {"id": "900", "name": "Example server"}}),
        encoding="utf-8",
    )
    path = channel / f"messages.{extension}"
    if extension == "json":
        path.write_text(json.dumps(rows), encoding="utf-8")
    else:
        with path.open("w", encoding="utf-8", newline="") as stream:
            writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
            writer.writeheader()
            writer.writerows(rows)


def preview(binary: Path, source: Path | str, arguments: list[str], expected: str, child: dict[str, str]) -> None:
    """Run one offline dry run and check that it matched the expected messages."""
    output = run([str(binary), "--no-tui", "--package", str(source), *arguments], child)
    if expected not in output or "Dry run: nothing was deleted." not in output:
        raise RuntimeError(f"The offline preview with arguments {arguments} did not print {expected!r}. Check package parsing and retry the build.")


def train(binary: Path, environment: dict[str, str], profiles: Path) -> None:
    """Exercise actual CLI parsing, JSON/CSV packages, filters, and previews."""
    with tempfile.TemporaryDirectory(prefix="discord-purge-training-") as temporary:
        fixture = Path(temporary)
        child = fixture_environment(environment, fixture) | {
            "LLVM_PROFILE_FILE": str(profiles / "training-%m-%p.profraw"),
        }
        rows: list[dict[str, str]] = [
            {"ID": str(1_300_000_000_000_000_000 + index),
             "Contents": f"Example message {index}",
             "Attachments": "https://example.invalid/image.png" if index % 2 else ""}
            for index in range(10_000)
        ]
        for extension in ("json", "csv"):
            write_package(fixture, extension, rows)
            for arguments, expected in (
                ([], "Matched 10000 message(s)"),
                (["--type", "image"], "Matched 5000 message(s)"),
                (["--content", "/Example message [0-9]+/i"], "Matched 10000 message(s)"),
            ):
                preview(binary, fixture / extension, arguments, expected, child)
        archive = shutil.make_archive(str(fixture / "package"), "zip", fixture / "json")
        preview(binary, archive, [], "Matched 10000 message(s)", child)


def parse_hot_count(summary: str) -> int:
    """Read the 95th-percentile block count from `llvm-profdata show --detailed-summary`."""
    match = re.search(r"with count >= (\d+) account for 95% of the total counts\.", summary)
    if match is None or int(match.group(1)) == 0:
        raise RuntimeError("llvm-profdata did not report a positive 95th-percentile count. Check that the training runs exit normally and that llvm-tools-preview matches the active Rust toolchain, then retry the build.")
    return int(match.group(1))


def package(binary: Path, target: str, output: Path, version: str, pgo: bool) -> Path:
    """Write the public binary, license, and README without local metadata."""
    output.mkdir(parents=True, exist_ok=True)
    name = f"{APP}-{target}"
    files = [binary, ROOT / "LICENSE", ROOT / "README.md"]
    archive = output / (name + (".zip" if "windows" in target else ".tar.gz"))
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
            for path in files:
                bundle.write(path, f"{name}/{path.name}")
    else:
        with tarfile.open(archive, "w:gz") as bundle:
            for path in files:
                info = bundle.gettarinfo(str(path), arcname=f"{name}/{path.name}")
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = ""
                with path.open("rb") as stream:
                    bundle.addfile(info, stream)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8",
    )
    (output / f"{name}.build.json").write_text(json.dumps({
        "version": version, "target": target, "pgo": pgo,
        "training": "offline JSON, CSV, ZIP previews and filters" if pgo else None,
        "sha256": digest,
    }, indent=2) + "\n", encoding="utf-8")
    return archive


def build() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target" / "pgo")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target" / "distrib")
    parser.add_argument("--tag", help="Release tag; must match the Cargo package version")
    parser.add_argument("--no-pgo", action="store_true", help="Windows ARM64 fallback only")
    parser.add_argument("--baseline-and-pgo", action="store_true",
                        help="Also build a matched binary without PGO for scripts/benchmark_pgo.py")
    args = parser.parse_args()
    if args.baseline_and_pgo and args.no_pgo:
        parser.error("You passed --baseline-and-pgo with --no-pgo, but a comparison needs a PGO build. Remove one of the two options.")
    environment = os.environ.copy()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if args.tag is not None and args.tag != f"v{version}":
        parser.error(f"Tag does not match Cargo version {version}. Use v{version}, or update Cargo.toml before tagging.")
    host = rust_host(environment)
    target = args.target or host
    if target not in TARGETS:
        parser.error("The Rust host is unsupported. Use a native runner for a listed --target.")
    runnable = target == host or (
        host.endswith("-unknown-linux-gnu")
        and target == host.removesuffix("-gnu") + "-musl"
    )
    if not runnable:
        parser.error(f"Target {target} cannot train on host {host}. Use a native runner for that target.")
    if args.no_pgo != (target == "aarch64-pc-windows-msvc"):
        parser.error("Use --no-pgo only for Windows ARM64, which has no Rust profiling runtime. Use PGO for other targets.")
    work = args.target_dir.resolve() / target
    profiles = work / "profiles"
    profiles.mkdir(parents=True, exist_ok=True)
    for path in profiles.glob("*.profraw"):
        path.unlink()
    # Environment flags replace Cargo config, so explicitly keep HTTP/3 enabled.
    flags = environment.get("CARGO_ENCODED_RUSTFLAGS")
    base_flags = flags.split("\x1f") if flags else shlex.split(environment.get("RUSTFLAGS", ""))
    base_flags += ["--cfg", "reqwest_unstable",
                   f"--remap-path-prefix={ROOT}=.",
                   f"--remap-path-prefix={Path.home()}=/build"]
    environment["CARGO_INCREMENTAL"] = "0"
    if target.endswith("-apple-darwin"):
        for variable in ("CFLAGS", "CXXFLAGS"):
            environment[variable] = " ".join(filter(None, (environment.get(variable), "-fno-profile-generate -fno-profile-use")))
    command = ["cargo", "build", "--locked", "--bin", APP, "--profile", "dist", "--target", target]
    if args.baseline_and_pgo:
        print(f"Build baseline {target}", flush=True)
        run(command, environment | {"CARGO_TARGET_DIR": str(work / "baseline"),
                                    "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(base_flags)})
    if not args.no_pgo:
        sysroot = Path(run(["rustc", "--print", "sysroot"], environment).strip())
        profiler = sysroot / "lib" / "rustlib" / host / "bin" / ("llvm-profdata.exe" if "windows" in host else "llvm-profdata")
        if not profiler.is_file():
            raise RuntimeError("LLVM tools are missing. Run `rustup component add llvm-tools-preview`, then retry the build.")
        instrumented = work / "instrumented"
        instrumented_env = environment | {
            "CARGO_TARGET_DIR": str(instrumented),
            "CARGO_ENCODED_RUSTFLAGS": "\x1f".join([*base_flags, f"-Cprofile-generate={profiles}"]),
        }
        print(f"Build and train {target}", flush=True)
        run(command, instrumented_env)
        train(instrumented / target / "dist" / binary_name(target), environment, profiles)
        raw = sorted(profiles.glob("*.profraw"))
        if not raw or any(path.stat().st_size == 0 for path in raw):
            raise RuntimeError("Training did not write complete profiles. Check that the offline runs exit normally, then retry.")
        merged = work / "merged.profdata"
        run([str(profiler), "merge", "-o", str(merged), *map(str, raw)], environment)
        hot_count = parse_hot_count(run([
            str(profiler), "show", "--detailed-summary", "--detailed-summary-cutoffs=950000", str(merged),
        ], environment))
        print(f"Merged {len(raw)} nonempty profiles, 95th-percentile hot count {hot_count}", flush=True)
        base_flags += [f"-Cprofile-use={merged}", f"-Cllvm-args=--profile-summary-hot-count={hot_count}"]
    optimized = work / "optimized"
    run(command, environment | {"CARGO_TARGET_DIR": str(optimized),
                               "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(base_flags)})
    binary = optimized / target / "dist" / binary_name(target)
    train(binary, environment, work / "verification")
    archive = package(binary, target, args.output_dir.resolve(), version, not args.no_pgo)
    print(f"Release archive: {archive}", flush=True)


if __name__ == "__main__":
    try:
        build()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Release build failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
