# GitHub Release 与自建 Homebrew tap

源码仓库：`alphadoiy/usage`；tap 仓库：`alphadoiy/homebrew-tap`。两者为公开仓库。安装命令遵循 [Homebrew tap 命名规则](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap)：

```sh
brew install alphadoiy/tap/usage
```

## 首次发布

先检查将要公开的源码、提交历史与 README，提交发布配置，并登录 GitHub CLI：

```sh
gh auth login
git add .github/workflows/release.yml scripts/release-formula.py scripts/publish-github.sh README.md docs/github-release.md
git commit -m "build: add GitHub releases and Homebrew tap publishing"
bash scripts/publish-github.sh
```

脚本要求干净工作区。它创建缺少的公开仓库，将当前提交推送到源码仓库的 `main`，创建与 Cargo 版本一致的标签（如 `v0.1.0`），等待 Release 工作流成功，再把 Release 内的 `usage.rb` 提交到 tap 的 `Formula/usage.rb`。已有远程分支与标签不会被强制覆盖。脚本需要 GitHub CLI 的仓库创建、源码及工作流写入权限；使用 classic token 时需包含 `repo` 和 `workflow` 权限。可设置 `USAGE_GITHUB_REPO` 与 `USAGE_HOMEBREW_REPO` 覆盖仓库名，README 的安装命令需同步修改。

Release 工作流在 [GitHub 的 macOS runner](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) 上构建 Apple Silicon（`aarch64-apple-darwin`）二进制，不再提供 Intel 构建。部署目标是 macOS 15。测试只在 `main` 的 CI 中对同一提交运行一次；`verify-ci` 与构建并行，等待该提交的 CI 成功，再由 `publish` 生成 SHA-256 校验文件和 Formula 并发布 Release。CI 并行运行 Rust 测试，进程级的后台任务预算在测试构建里按线程隔离，避免并行测试互相占用许可。没有成功的 CI 时发布失败，因此需要先推送到 `main`（发布脚本会这样做）。Formula 带 `depends_on arch: :arm64`，Intel Mac 会收到明确的安装错误。运行时只安装预编译二进制，无需 Rust。tap 更新使用本机已有 GitHub 登录，不需要向 Actions 另存跨仓库 token。

`main` 的 CI 会预热 release 依赖缓存，标签构建使用相同的缓存键读取。GitHub 标签之间的缓存互相隔离，因此缓存写入默认分支，发布任务只读取；首次预热仍需完整编译。缓存按 Rust 工具链、目标架构、依赖及编译环境区分，业务源码会重新编译。

## 后续更新

修改 `Cargo.toml` 的版本号，运行 `cargo check --locked` 前先用 `cargo check --offline` 更新 `Cargo.lock` 的本包版本，然后提交代码，运行：

```sh
bash scripts/publish-github.sh
```

每个版本使用新标签，不覆盖已发布归档。脚本可在已成功发布的同一提交上重跑以补推 tap。构建失败时，在 GitHub Actions 中修复或重新运行失败任务，成功后重跑脚本。若已有未完成的草稿 Release，应先处理草稿再重新运行发布任务。

用户更新：

```sh
brew update
brew upgrade alphadoiy/tap/usage
```

本机 `usage-local` 的打包流程仍见[本机 Homebrew 安装](homebrew-local.md)。发布脚本不会安装、卸载或替换当前机器上的程序。
