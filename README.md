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

用量卡片会在额度旁显示当前账户的邮箱（如果能够确定）：Claude Code 使用同一 OAuth 令牌查询账户资料，Codex 从当前 ChatGPT 登录的 ID Token 读取，Grok 从选中的登录条目或其令牌读取。邮箱缺失或资料查询失败时不显示；邮箱不会写入配置文件或状态栏缓存。

## 安装

需要 Rust 1.88 或更新版本。

只在本机使用时，可交给 Homebrew 管理。打包脚本用当前源码构建 macOS 二进制，生成 Formula 并放入本机 `alphadoiy/local` tap；无需 GitHub 仓库或发布版本。具体步骤和产物说明见[本机 Homebrew 安装与更新](docs/homebrew-local.md)。

```bash
./scripts/package-brew-local.sh
brew install alphadoiy/local/usage-local
usage
```

修改源码后重新打包并安装；移除时用 `brew uninstall usage-local`。Formula 的名称是 `usage-local`，安装的命令仍是 `usage`。本地源码变化不会被 `brew upgrade` 自动发现。

```bash
./scripts/package-brew-local.sh
brew reinstall alphadoiy/local/usage-local
```

也可以直接通过 Cargo 安装：

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
usage history         # TUI 贡献看板（也可用 usage dashboard）
usage history --providers codex,claude --monthly
usage history --year 2026 --json  # 导出日/月 Token 汇总
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
| `h` | 打开本地 Token 贡献看板 |
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

## TUI 贡献看板

主面板按 `h`，或运行 `usage history` / `usage dashboard`，进入终端内的历史用量看板。布局按“年度概览 → 贡献图 → 选中日期与近期用量”排列：来源筛选放在顶部，年度摘要显示 Token、活跃日和缓存读取占比。

贡献图参考 [hindsight](https://github.com/chaosprint/hindsight) 的前景色小方块样式，列间留空，星期只标周一/周三/周五。蓝色阶按当前年份、当前来源的活跃用量划分，使峰值很高时较小用量也容易区分；零用量显示灰色方块，未来日期留空。选中日期显示 `▣`，下方集中展示输入、输出、缓存和来源分布。窄终端只显示选中日期附近的周；宽终端将选中日期明细与近期列表并排。

近期列表最多显示 7 条，跳过未选中的零用量日期，不随终端高度铺满。按 `PgUp` / `PgDn` 跳到前一个/后一个活跃日，可快速浏览稀疏历史；普通方向键仍能选中无用量的过去日期。

按 `m` 切换月度视图：显示选中月份的输入、输出、缓存及来源合计，与 12 个月的彩色堆叠趋势条；不再重复堆叠月度表格。蓝色为 Codex、暖色为 Claude、灰白色为 Grok。较矮终端优先显示月度趋势，较高终端同时保留热力图。

| 键 | 作用 |
| --- | --- |
| `0` / `1` / `2` / `3` | 全部 / Codex / Claude / Grok（在本次 `--providers` 范围内） |
| `Tab` | 轮换来源筛选 |
| `m` | 每日 / 月度视图 |
| `↑` / `↓` 或 `k` / `j` | 每日视图选择日期，月度视图选择月份 |
| `←` / `→` | 前一周 / 后一周 |
| `PgUp` / `PgDn` | 前一个 / 后一个活跃日 |
| `[` / `]` | 上一年 / 下一年 |
| `t` | 回到今天 |
| `r` | 重新读取日志 |
| `Esc` / `q` | 返回主面板；独立打开时退出 |

```bash
usage history --year 2026
usage dashboard --monthly --providers codex,claude,grok
usage history --providers grok --monthly --once
usage history --json
```

历史数据参考 cockpit-tools 的会话日志统计方式，读取配置中的本地目录：Codex 的 `sessions/` 和 `archived_sessions/`、Claude 的 `projects/`（包括子代理日志）、Grok 的 `sessions/**/updates.jsonl`。读取在后台进行；按 `r` 刷新时复用未变化文件的内存缓存，变化的文件重新统计。

Codex 优先读取 `last_token_usage`，缺失时使用累计值的增量，去掉重复额度快照和分叉会话重放前缀；Claude 按消息 ID 合并流式记录；Grok 按 prompt ID 去重，只统计 `turn_completed` 的汇总。日期按本地时区归属。总 Token 为输入加输出，缓存已经包含在输入内，不会重复相加；Claude 的输入包含缓存读取和缓存写入，Codex/Grok 的缓存读取是输入的子集。Grok 的调用数来自 `modelCalls`，Codex/Claude 则为有效用量记录数，三者不能视为严格一致的 HTTP 请求次数。

此看板统计本机仍保留的日志，已删除或未记录的历史无法还原；它与主面板的官方滚动额度百分比是不同的数据。`--json` 输出全年日/月汇总（包括零用量日期）和读取警告，不包含对话内容、登录凭据或项目路径。

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
