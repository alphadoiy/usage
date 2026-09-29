# usage

在终端里查看 Claude Code、Codex 和 Grok 的官方已用额度。百分比是已经用掉的比例。

界面用 [ratatui](https://ratatui.rs/) 绘制，单次只显示一种用量图：

- Braille 点阵：额度窗口之间留一行，依次显示名称、彩色点阵进度条、百分比和重置倒计时
- `Canvas` 上的 `Circle` 和 `Line`：用量圆环
- 圆点状态栏：参考 [nilbuild/claude-statusline](https://github.com/nilbuild/claude-statusline) 的短圆点条，圆点之间留空格，额度窗口之间留一行；Claude、Codex、Grok 都可使用

终端够宽时，Claude Code、Codex 和 Grok 三张卡片并排显示；较窄时自动改为两列或一列。Braille 进度条会随卡片宽度延伸；窄卡片会把额度信息和进度条分成两行，避免截断。

查询方式对齐 cc-switch 的官方订阅额度：只读取本机已有登录凭据，不负责登录，也不把令牌写回磁盘。

| 工具 | 凭据 | 接口 |
| --- | --- | --- |
| Claude Code | Keychain `Claude Code-credentials`，否则 `~/.claude/.credentials.json` | `GET https://api.anthropic.com/api/oauth/usage` |
| Codex | Keychain `Codex Auth`，否则 `~/.codex/auth.json`（仅 ChatGPT 登录） | `GET https://chatgpt.com/backend-api/wham/usage`；主动重置走 `rate-limit-reset-credits` 接口 |
| Grok | `~/.grok/auth.json`，优先 SuperGrok OIDC 条目 | `POST https://grok.com/.../GetGrokCreditsConfig` |

## 安装

需要 Rust 1.88 或更新版本。

```bash
cargo build --release
cargo install --path .
```

开发时可以直接：

```bash
cargo run
cargo run -- --once
```

## 使用

```bash
usage                 # 打开 TUI
usage --once          # 查询一次，打印文本
usage --json          # 查询一次，打印 JSON
usage statusline --providers claude  # 给 Claude Code 等 agent 输出轻量状态栏
usage --style ring    # 本次只用圆环
usage --style statusline  # 本次使用圆点状态栏视图
usage --providers claude,codex
usage providers       # 列出已注册的工具
usage styles          # 列出可视化样式
usage init-config     # 写出 ~/.config/usage/config.toml
```

`--once` 和 `--json` 会并发查询所选工具，输出仍按 `providers` 的配置顺序排列。

TUI 按键：

| 键 | 作用 |
| --- | --- |
| `r` | 刷新 |
| `1` | Braille 进度条 |
| `2` | 圆环（Canvas） |
| `3` | 圆点状态栏 |
| `s` | 把当前样式写进配置 |
| `x` | 查看 Codex 可用的主动重置次数，确认后重置当前 5 小时额度 |
| `p` | 浏览 Claude Code、Codex、Grok 的本机会话，在 Ghostty 右侧分屏恢复 |
| `j` / `k` | 滚动 |
| `q` | 退出 |

没有配置文件也能直接运行。默认每 60 秒刷新一次，三个工具都查询，样式是 Braille 进度条。Claude 使用暖色、Codex 使用蓝色、Grok 使用黑白。Claude 和 Codex 的用量达到 70% 时变橙色、90% 时变红色；Grok 始终保持黑白。旧配置中的 `style = "both"` 读取时会转成进度条。

```toml
style = "bar"
refresh_seconds = 60
providers = ["claude", "codex", "grok"]

[paths]
claude = ""
codex = ""
grok = ""
```

令牌过期时，界面会提示重新登录对应的 CLI。网络抖动或查询超时时保留上一份成功结果。TUI 空闲时按秒更新倒计时，查询结果和按键变化会立即重画。TUI 和一次性查询对单个工具最多等待 20 秒；Keychain 读取最多等待 10 秒。独立状态栏对单个工具最多等待 4 秒。

Codex 的用量响应若提供主动重置次数，卡片会显示剩余次数。按 `x` 后会向 Codex 查询最新次数和最近到期时间；确认框显示当前账号，按 `y` 才会消耗一次，按 `n` 或 `Esc` 取消。提交前会再次确认有可用次数，成功后刷新额度。若提交请求中断或服务端暂时不可用，结果可能无法判定；先刷新额度和剩余次数，再决定是否重试。Claude Code 和 Grok 没有主动重置入口。

在 Ghostty 中按 `p` 打开会话浏览器。第一级按最近活动时间列出项目，第二级显示该项目的会话，并按 Claude Code、Codex、Grok 分组。宽终端左右排列，窄终端上下排列。用方向键或 `j`/`k` 选择项目，按 `Enter` 进入会话列表；按 `Tab` 或左右方向键切换列表。选中会话后，底部会预览工具、项目目录和即将运行的恢复命令；再按 `Enter`，会在当前终端右侧新建 split，进入项目目录并恢复会话。按 `/` 搜索项目路径、标题、工具或会话 ID，按 `c` 清除搜索，按 `r` 重新读取。`Esc` 从会话返回项目，再按一次返回用量面板。新 split 会沿用对应工具的配置目录。此功能要求 macOS Ghostty 1.3+ 和对应工具的 CLI；Codex 优先读取本机 SQLite 索引，无法打开时改读本机会话日志。首次使用时 macOS 可能要求允许终端控制 Ghostty。列表只收录 ID 合法且本机项目目录仍存在的会话；实际恢复结果由对应工具的 CLI 决定。

会话扫描在后台运行，刷新时保留当前项目和会话选择。标题优先使用真实的首条用户提示，跳过工具结果和自动注入的上下文；Grok 子代理不会作为独立会话列出，分叉会话仍可选择。Codex 日志回退只扫描 `sessions/`，并排除在 `archived_sessions/` 中也能找到的会话。新增会话来源可向 `SessionRegistry` 注册 `SessionSource`，用 `SessionHomes::with` 指定目录，再通过 `ui::run_with_sessions` 注入；来源的名称、颜色、排序、恢复命令和环境变量由 `AgentKind::new` 一起定义，浏览器无需增加工具分支。

## 状态栏

`usage --style statusline` 是面板中的圆点 UI，适用于所有已注册工具。按 `3` 可以直接切换，按 `s` 保存。例如 `5小时  ● ● ● ● ● ○ ○ ○  57%  ⟳ 2h0m`。

另有 `usage statusline` 命令输出形态，适合嵌入 Claude Code 的底部状态栏。它读取 Claude Code 传入的 JSON，显示模型、上下文占用、目录和 Git 分支；若 JSON 已带额度窗口，直接使用这些数字。否则通过本工具的 provider 查询，并在 `~/.cache/usage/statusline.json` 缓存 60 秒。缓存只包含用量快照，不包含登录令牌。

先执行 `cargo install --path .`，再将以下字段合并进 `~/.claude/settings.json`：

```json
{
  "statusLine": {
    "type": "command",
    "command": "$HOME/.cargo/bin/usage statusline --providers claude"
  }
}
```

状态栏每个额度窗口占一行，例如 `Claude Code 5h ● ● ● ● ○ ○ ○ ○ 50% ⟳ 2h`。可将 `--providers` 设为 `claude,codex,grok` 来显示其他工具；`--cache-seconds 0` 会每次重新查询，`--no-color` 会关闭 ANSI 颜色。命令也可单独运行，不带 stdin 时仅显示额度。

## 扩展

工具和画法都是注册进去的，界面只认识统一的用量快照。

新增一个工具：实现 `UsageProvider`，在 `builtin_factory()` 里登记工厂。

```rust
factory.register("gemini", |ctx| Box::new(GeminiProvider::new(ctx)))?;
```

工具若支持主动重置，可另外实现 `UsageReset`，并让其 `UsageProvider::reset()` 返回该能力；默认提供方不提供重置。

新增一种画法：向 `StyleRegistry` 登记一个函数。它把快照中的额度窗口编排为一种视图；内置的进度条使用 Braille 点阵，圆环使用 `Canvas`。

```rust
registry.register("spark", "另一种", render_spark);
```

注册之后，`--style`、配置文件和 TUI 都能选到它。
