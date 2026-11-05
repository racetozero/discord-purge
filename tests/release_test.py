"""Check release archives and both installers with local file:// releases."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parent.parent
TAG = "v0.1.0"
POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")


def write_release(releases: Path, target: str, payload: bytes, checksum: str | None = None) -> None:
    """Write one invented release archive and its .sha256 file."""
    directory = releases / "download" / TAG
    directory.mkdir(parents=True, exist_ok=True)
    windows = "windows" in target
    archive = directory / f"discord-purge-{target}{'.zip' if windows else '.tar.gz'}"
    member = f"discord-purge-{target}/discord-purge{'.exe' if windows else ''}"
    if windows:
        with zipfile.ZipFile(archive, "w") as bundle:
            bundle.writestr(member, payload)
    else:
        with tarfile.open(archive, "w:gz") as bundle:
            info = tarfile.TarInfo(member)
            info.size, info.mode = len(payload), 0o755
            bundle.addfile(info, io.BytesIO(payload))
    digest = checksum or hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")


class InstallerTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="discord-purge-release-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.releases = self.root / "releases"
        self.install = self.root / "bin"

    def run_installer(self, target: str) -> subprocess.CompletedProcess[str]:
        environment = {
            key: value for key, value in os.environ.items() if not key.startswith("DISCORD_PURGE_")
        } | {
            "HOME": str(self.root),
            "DISCORD_PURGE_VERSION": TAG,
            "DISCORD_PURGE_TARGET": target,
            "DISCORD_PURGE_INSTALL_DIR": str(self.install),
            "DISCORD_PURGE_RELEASES_URL": self.releases.as_uri(),
        }
        if "windows" in target:
            command = [POWERSHELL, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(ROOT / "install.ps1")]
        else:
            command = ["sh", str(ROOT / "install.sh")]
        return subprocess.run(command, env=environment, text=True, capture_output=True, check=False)

    def check_installer(self, target: str, binary: str) -> None:
        installed = self.install / binary

        write_release(self.releases, target, b"first")
        result = self.run_installer(target)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(installed.read_bytes(), b"first")

        write_release(self.releases, target, b"second")
        result = self.run_installer(target)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(installed.read_bytes(), b"second")

        write_release(self.releases, target, b"tampered", checksum="0" * 64)
        result = self.run_installer(target)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum verification failed", result.stderr)
        self.assertEqual(installed.read_bytes(), b"second", "a failed check must keep the installed binary")

        result = self.run_installer("riscv64-unknown-plan9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("riscv64-unknown-plan9", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "install.sh runs on macOS and Linux")
    def test_shell_installer(self) -> None:
        self.check_installer("x86_64-unknown-linux-musl", "discord-purge")
        if os.name == "posix":
            self.assertTrue(os.access(self.install / "discord-purge", os.X_OK))

    @unittest.skipUnless(POWERSHELL and sys.platform == "win32", "install.ps1 runs on Windows")
    def test_powershell_installer(self) -> None:
        self.check_installer("x86_64-pc-windows-msvc", "discord-purge.exe")
        write_release(self.releases, "aarch64-pc-windows-msvc", b"arm")
        result = self.run_installer("aarch64-pc-windows-msvc")
        self.assertEqual(result.returncode, 0, result.stderr)


def load_build_release() -> ModuleType:
    specification = importlib.util.spec_from_file_location("build_release", ROOT / "scripts" / "build_release.py")
    assert specification is not None and specification.loader is not None
    build_release = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(build_release)
    return build_release


class PgoTest(unittest.TestCase):
    def test_hot_count_must_be_a_positive_llvm_value(self) -> None:
        build_release = load_build_release()
        summary = "123 blocks (5.00%) with count >= 41 account for 95% of the total counts."
        self.assertEqual(build_release.parse_hot_count(summary), 41)
        for invalid in ("no detailed summary", summary.replace(">= 41", ">= 0")):
            with self.assertRaises(RuntimeError):
                build_release.parse_hot_count(invalid)

    def test_training_never_receives_credentials(self) -> None:
        fixture = Path("fixture")
        child: dict[str, str] = load_build_release().fixture_environment({
            "DISCORD_TOKEN": "private", "DISCORD_PURGE_STATE_DIR": "/host/state",
            "DISCORD_PURGE_IDENTITY_DIR": "/host/identity", "GH_TOKEN": "private",
            "SOME_PASSWORD": "private", "A_SECRET_VALUE": "private", "PATH": "/usr/bin",
        }, fixture)
        self.assertEqual(child, {
            "PATH": "/usr/bin",
            "DISCORD_PURGE_STATE_DIR": str(fixture / "state"),
            "DISCORD_PURGE_IDENTITY_DIR": str(fixture / "identity"),
        })


class PackageTest(unittest.TestCase):
    def test_archive_contents_and_checksum(self) -> None:
        build_release = load_build_release()
        with tempfile.TemporaryDirectory(prefix="discord-purge-package-test-") as temporary:
            output = Path(temporary)
            binary = output / "discord-purge"
            binary.write_bytes(b"binary")
            for target, suffix in (("x86_64-unknown-linux-gnu", ".tar.gz"), ("x86_64-pc-windows-msvc", ".zip")):
                archive: Path = build_release.package(binary, target, output / "dist", "0.1.0", True)
                self.assertEqual(archive.name, f"discord-purge-{target}{suffix}")
                if suffix == ".zip":
                    with zipfile.ZipFile(archive) as bundle:
                        names = bundle.namelist()
                else:
                    with tarfile.open(archive) as bundle:
                        names = bundle.getnames()
                        self.assertTrue(all(member.uid == 0 and member.uname == "" for member in bundle.getmembers()))
                prefix = f"discord-purge-{target}/"
                self.assertEqual(sorted(names), [prefix + "LICENSE", prefix + "README.md", prefix + "discord-purge"])
                digest, name = archive.with_name(archive.name + ".sha256").read_text(encoding="utf-8").split()
                self.assertEqual(name, archive.name)
                self.assertEqual(digest, hashlib.sha256(archive.read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
