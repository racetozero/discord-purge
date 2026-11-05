"""Reject tracked files that contain captures, credentials, emails, or home paths, and commits without a noreply email."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PRIVATE_NAMES = re.compile(r"(^|/)(.*\.har(\..*)?|\.client-identity\.json|client-profile\.json|token(\.txt)?|.*\.deleted\.log|report-.*\.txt|\.env(\..*)?|AGENTS\.md|CLAUDE\.md)$|(^|/)\.agent/", re.IGNORECASE)
PRIVATE_TEXT = {
    "email address": re.compile(r"(?<![\w.+-])(?!noreply@github\.com(?![\w-]|\.[\w-]))[\w.+-]+@(?!users\.noreply\.github\.com|example\.|[\w-]+\.invalid)[\w-]+(\.[\w-]+)*\.[a-z]{2,}", re.IGNORECASE),
    "home directory path": re.compile(r"/Users/(?!Shared)[\w.-]+/|/home/(?!runner/)[\w.-]+/|[A-Z]:\\Users\\[\w.-]+\\", re.IGNORECASE),
    "Discord token": re.compile(r"\b[MNO][\w-]{23,27}\.[\w-]{6}\.[\w-]{27,}|\bmfa\.[\w-]{80,}"),
}

files = subprocess.run(
    ["git", "ls-files", "--cached", "--others", "--exclude-standard"],
    cwd=ROOT, check=True, text=True, capture_output=True,
).stdout.splitlines()
problems: list[str] = []
for name in files:
    if PRIVATE_NAMES.search(name):
        problems.append(f"{name}: private local file")
        continue
    path = ROOT / name
    if not path.is_file() or path.suffix in {".zip", ".lock"}:
        continue
    text = path.read_text(encoding="utf-8", errors="ignore")
    for number, line in enumerate(text.splitlines(), 1):
        problems += [f"{name}:{number}: {kind}" for kind, pattern in PRIVATE_TEXT.items() if pattern.search(line)]

commits = subprocess.run(
    ["git", "log", "--format=%h %ae %ce"],
    cwd=ROOT, check=True, text=True, capture_output=True,
).stdout.splitlines()
for commit in commits:
    sha, author, committer = commit.split()
    # The GitHub web editor commits with this shared committer address.
    if not author.endswith("@users.noreply.github.com") or not (committer.endswith("@users.noreply.github.com") or committer == "noreply@github.com"):
        problems.append(f"commit {sha}: the author or committer email is not allowed. Use a @users.noreply.github.com author and a @users.noreply.github.com or noreply@github.com committer: set user.email in this clone, then amend the commit")

if problems:
    print("Publication check failed. These tracked files contain private data:", *problems, sep="\n  ", file=sys.stderr)
    print("Remove the value or replace it with an invented example, add private files to .gitignore, then run the check again.", file=sys.stderr)
    raise SystemExit(1)
print(f"Publication check passed for {len(files)} files.")
