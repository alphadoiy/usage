#!/usr/bin/env bash
# Publish a stable release, then update a separate public Homebrew tap.
set -euo pipefail
cd "$(dirname "$0")/.."

repo="${USAGE_GITHUB_REPO:-alphadoiy/usage}"
tap="${USAGE_HOMEBREW_REPO:-alphadoiy/homebrew-tap}"
for name in "$repo" "$tap"; do
  if [[ ! "$name" =~ ^[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+$ ]]; then
    echo "Invalid repository: $name" >&2
    exit 1
  fi
done
gh auth status
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Commit the reviewed changes before publishing." >&2
  exit 1
fi
version="$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -n 1)"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Cargo.toml must contain a stable MAJOR.MINOR.PATCH version." >&2
  exit 1
fi
tag="v$version"
if [[ "$(gh config get git_protocol --host github.com)" == ssh ]]; then
  git_url="git@github.com:$repo.git"
else
  gh auth setup-git
  git_url="https://github.com/$repo.git"
fi

if ! gh repo view "$repo" >/dev/null 2>&1; then
  gh repo create "$repo" --public --description "Terminal dashboard for AI agent usage, providers, and sessions"
fi
if ! git remote get-url origin >/dev/null 2>&1; then
  git remote add origin "$git_url"
fi
# Use explicit destinations; never force-push an existing branch or tag.
git push "$git_url" HEAD:refs/heads/main
gh repo edit "$repo" --default-branch main
if git rev-parse "$tag" >/dev/null 2>&1; then
  if [[ "$(git rev-list -n 1 "$tag")" != "$(git rev-parse HEAD)" ]]; then
    echo "$tag already identifies another commit; bump Cargo.toml and Cargo.lock." >&2
    exit 1
  fi
else
  git tag -a "$tag" -m "Release $tag"
fi
git push "$git_url" "refs/tags/$tag"

if ! gh release view "$tag" --repo "$repo" --json isDraft --jq '.isDraft == false' | grep -qx true; then
  run_id=""
  for attempt in {1..30}; do
    run_id="$(gh run list --repo "$repo" --workflow release.yml --branch "$tag" --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
    [[ -n "$run_id" ]] && break
    sleep 2
  done
  if [[ -z "$run_id" ]]; then
    echo "No release workflow found. Check https://github.com/$repo/actions" >&2
    exit 1
  fi
  gh run watch "$run_id" --repo "$repo" --exit-status --interval 15
fi

mkdir -p dist
stage="$(mktemp -d "${TMPDIR:-/tmp}/usage-publish.XXXXXX")"
gh release download "$tag" --repo "$repo" --pattern usage.rb --dir "$stage"
ruby -c "$stage/usage.rb"
if ! gh repo view "$tap" >/dev/null 2>&1; then
  gh repo create "$tap" --public --description "Homebrew formulae by ${tap%%/*}"
fi
gh repo clone "$tap" "$stage/tap"
mkdir -p "$stage/tap/Formula"
cp "$stage/usage.rb" "$stage/tap/Formula/usage.rb"
if [[ ! -f "$stage/tap/README.md" ]]; then
  printf '# Homebrew tap\n\nInstall Usage:\n\n```sh\nbrew install %s/usage\n```\n' "${tap/\/homebrew-/\/}" > "$stage/tap/README.md"
fi
git -C "$stage/tap" add Formula/usage.rb README.md
if ! git -C "$stage/tap" diff --cached --quiet; then
  git -C "$stage/tap" commit -m "usage $version"
  git -C "$stage/tap" push origin HEAD:main
  gh repo edit "$tap" --default-branch main
fi
echo "Release: https://github.com/$repo/releases/tag/$tag"
echo "Install: brew install ${tap/\/homebrew-/\/}/usage"
