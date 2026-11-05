"""Print the Homebrew formula of one release, using its .sha256 files."""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPOSITORY = "racetozero/discord-purge"
# Homebrew does not pick a libc variant, and the glibc builds need a recent
# glibc, so Linux uses the static musl builds.
TARGETS = {
    "MACOS_ARM": "aarch64-apple-darwin",
    "MACOS_INTEL": "x86_64-apple-darwin",
    "LINUX_ARM": "aarch64-unknown-linux-musl",
    "LINUX_INTEL": "x86_64-unknown-linux-musl",
}
TEMPLATE = '''class DiscordPurge < Formula
  desc "Delete your own Discord messages from a TUI, web panel, or script"
  homepage "https://github.com/{repository}"
  version "{version}"
  license "MIT"

  on_macos do
    on_arm do
      url "{MACOS_ARM_URL}"
      sha256 "{MACOS_ARM_SHA}"
    end
    on_intel do
      url "{MACOS_INTEL_URL}"
      sha256 "{MACOS_INTEL_SHA}"
    end
  end

  on_linux do
    on_arm do
      url "{LINUX_ARM_URL}"
      sha256 "{LINUX_ARM_SHA}"
    end
    on_intel do
      url "{LINUX_INTEL_URL}"
      sha256 "{LINUX_INTEL_SHA}"
    end
  end

  def install
    bin.install "discord-purge"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/discord-purge --version")
  end
end
'''


def formula(tag: str, artifacts: Path) -> str:
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise RuntimeError(f"The release tag is {tag!r}. Use a tag such as v1.2.3.")
    values: dict[str, str] = {"repository": REPOSITORY, "version": tag.removeprefix("v")}
    for key, target in TARGETS.items():
        archive = f"discord-purge-{target}.tar.gz"
        checksum_file = artifacts / f"{archive}.sha256"
        if not checksum_file.is_file():
            raise RuntimeError(f"{checksum_file} is missing. Publish the formula only after every target builds.")
        digest = checksum_file.read_text(encoding="utf-8").split()[0]
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise RuntimeError(f"{checksum_file} has no valid SHA-256 value. Build the release again.")
        values[f"{key}_URL"] = f"https://github.com/{REPOSITORY}/releases/download/{tag}/{archive}"
        values[f"{key}_SHA"] = digest
    return TEMPLATE.format(**values)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit("You gave the wrong arguments. Use: homebrew_formula.py v1.2.3 ARTIFACT_FOLDER")
    try:
        print(formula(sys.argv[1], Path(sys.argv[2])), end="")
    except (OSError, RuntimeError) as error:
        sys.exit(f"Formula failed: {error}")
