# 本机 Homebrew 安装与更新

这个方案把当前仓库的 Rust 程序交给 Homebrew 安装和卸载，适合只在自己的 Mac 上使用。它使用本地归档和本地 tap，不需要发布 GitHub Release。Formula 名为 `usage-local`，安装后的命令名仍是 `usage`。

## 准备

- macOS 和 Homebrew
- Rust 1.88 或更新版本，提供 `cargo` 和 `rustc`
- 本仓库的源码与 `Cargo.lock`

第一次安装，在仓库根目录运行：

```bash
./scripts/package-brew-local.sh
brew install alphadoiy/local/usage-local
usage --help
```

## 打包脚本做了什么

[`scripts/package-brew-local.sh`](../scripts/package-brew-local.sh) 依次执行以下操作：

1. 确认运行环境是 macOS，并用 `cargo build --release --locked` 构建 `target/release/usage`。`--locked` 要求依赖与 `Cargo.lock` 一致。
2. 从 `Cargo.toml` 读取版本，从 `rustc -vV` 读取目标平台，并计算可执行文件的 SHA-256。二进制校验值的前 12 位会放进归档文件名，使同一版本的不同构建有不同的文件名。
3. 把可执行文件打成 `dist/usage-v<版本>-<平台>-<校验值>.tar.gz`，计算归档的 SHA-256，然后生成 `dist/usage-local.rb`。Formula 用 `file://` 指向这个归档，用 SHA-256 校验内容，并把归档中的 `usage` 安装到 Homebrew 的 `bin` 目录。
4. 如有需要，创建不关联 Git 仓库的本地 tap `alphadoiy/local`，再把 Formula 复制到该 tap 的 `Formula/usage-local.rb`。tap 的实际目录由 `brew --repository alphadoiy/local` 查询，因此不依赖固定的 Homebrew 安装路径。

`dist/` 和 `target/` 都被 Git 忽略。打包脚本不会改动登录凭据或用量配置。安装完成后，可用以下命令查看路径：

```bash
brew --repository alphadoiy/local
brew --prefix usage-local
command -v usage
```

## 修改源码后更新

```bash
./scripts/package-brew-local.sh
brew reinstall alphadoiy/local/usage-local
```

先重新打包，才能让 Formula 指向新归档。即使 `Cargo.toml` 中的版本号不变，只要二进制内容变化，归档文件名中的校验值也会变化；使用 `brew reinstall` 安装它。本地源码变化不会自动触发 `brew upgrade`。请保留 `dist/` 中当前 Formula 引用的归档，否则之后的重新安装会找不到 `file://` 源文件；重新运行打包脚本可以生成新的归档并更新 Formula。

## 卸载

```bash
brew uninstall usage-local
```

这只移除 Homebrew 安装的程序。若不再需要本地 tap，可以在卸载后运行 `brew untap alphadoiy/local`。`dist/`、源码以及 `~/.config/usage/config.toml` 不会随 Homebrew 卸载而删除。

## 范围

Formula 通过本机绝对路径引用归档，因此这个 tap 只供当前机器使用。要换一台 Mac，请在那台机器上检出源码并重新运行打包脚本。构建需要 Rust；安装和运行已打包的程序不需要 Cargo。
