#!/usr/bin/env python3
"""Generate a Homebrew formula from the Apple Silicon release archive (Python 3.11+)."""
import argparse
import hashlib
from pathlib import Path
import re
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="alphadoiy/usage")
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        parser.error("--repo must be OWNER/REPOSITORY")
    version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        parser.error("stable releases require a MAJOR.MINOR.PATCH version")
    name = f"usage-v{version}-aarch64-apple-darwin.tar.gz"
    digest = hashlib.sha256((args.dist / name).read_bytes()).hexdigest()
    formula = f'''class Usage < Formula
  desc "Terminal dashboard for AI agent usage, providers, and sessions"
  homepage "https://github.com/{args.repo}"
  url "https://github.com/{args.repo}/releases/download/v{version}/{name}"
  sha256 "{digest}"

  depends_on arch: :arm64
  depends_on macos: :sequoia
  conflicts_with "usage-local", because: "both install the usage command"

  def install
    bin.install "usage"
  end

  test do
    assert_match "usage", shell_output("#{{bin}}/usage --help")
  end
end
'''
    (args.dist / "usage.rb").write_text(formula)


if __name__ == "__main__":
    main()
