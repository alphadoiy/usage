#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="$repo_dir/dist"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "本地 Homebrew 包目前只支持 macOS" >&2
  exit 1
fi

cd "$repo_dir"
cargo build --release --locked

version="$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -n 1)"
host="$(rustc -vV | awk '/^host:/ { print $2 }')"
binary_sha="$(shasum -a 256 target/release/usage | awk '{ print $1 }')"

if [[ -z "$version" || -z "$host" || -z "$binary_sha" ]]; then
  echo "无法读取版本、目标平台或二进制校验值" >&2
  exit 1
fi

mkdir -p "$dist_dir"
archive="$dist_dir/usage-v${version}-${host}-${binary_sha:0:12}.tar.gz"
tar -czf "$archive" -C target/release usage
archive_sha="$(shasum -a 256 "$archive" | awk '{ print $1 }')"

cat > "$dist_dir/usage-local.rb" <<FORMULA
class UsageLocal < Formula
  desc "Terminal usage dashboard for Claude Code, Codex, and Grok"
  homepage "https://github.com/alphadoiy"
  url "file://$archive"
  version "$version"
  sha256 "$archive_sha"

  depends_on :macos

  def install
    bin.install "usage"
  end

  test do
    system "#{bin}/usage", "--help"
  end
end
FORMULA

tap_name="alphadoiy/local"
tap_dir="$(brew --repository "$tap_name" 2>/dev/null || true)"
if [[ -z "$tap_dir" || ! -d "$tap_dir" ]]; then
  brew tap-new --no-git "$tap_name"
  tap_dir="$(brew --repository "$tap_name")"
fi
mkdir -p "$tap_dir/Formula"
cp "$dist_dir/usage-local.rb" "$tap_dir/Formula/usage-local.rb"

echo "已更新本地 Formula：$tap_name/usage-local"
echo "安装：brew install $tap_name/usage-local"
echo "更新：brew reinstall $tap_name/usage-local"
