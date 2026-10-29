# usage

在终端里查看托管 Codex 运行、BYOK 余额与费用，以及 Claude Code、Codex 和 Grok 的官方订阅额度。额度百分比是已经用掉的比例。

首页顶部用紧凑状态栏展示官方 API 厂商状态，宽屏主体并排显示 Provider 和项目会话，窄屏主体上下排列。Provider 优先展示用量／余额，再显示类型、配置和模型；厂商状态按 Enter 展开事件与历史，Esc 收回状态栏。

界面用 [ratatui](https://ratatui.rs/) 绘制，订阅额度卡片单次使用一种用量图：

- Braille 点阵：额度窗口之间留一行，依次显示名称、彩色点阵进度条、百分比和重置倒计时
- `Canvas` 上的 `Circle` 和 `Line`：用量圆环
- 圆点状态栏：参考 [nilbuild/claude-statusline](https://github.com/nilbuild/claude-statusline) 的短圆点条，圆点之间留空格，额度窗口之间留一行；Claude、Codex、Grok 都可使用

终端够宽时，Claude Code、Codex 和 Grok 三张卡片并排显示；较窄时自动改为两列或一列。Braille 进度条会随卡片宽度延伸；窄卡片会把额度信息和进度条分成两行，避免截断。

官方订阅额度查询对齐 cc-switch：只读取本机已有登录凭据，不负责登录，也不把订阅令牌写回磁盘。统一的 Provider 管理入口支持 BYOK、OAuth、WorkBuddy、Trae 和 ZCode，可保存配置和登录、导入 CC Switch 供应商并配置 Agent 启动。

| 工具 | 凭据 | 接口 |
| --- | --- | --- |
| Claude Code | Keychain `Claude Code-credentials`，否则 `~/.claude/.credentials.json` | `GET https://api.anthropic.com/api/oauth/usage` |
| Codex | Keychain `Codex Auth`，否则 `~/.codex/auth.json`（仅 ChatGPT 登录） | `GET https://chatgpt.com/backend-api/wham/usage`；主动重置走 `rate-limit-reset-credits` 接口 |
| Grok | `~/.grok/auth.json`，优先 SuperGrok OIDC 条目 | `POST https://grok.com/.../GetGrokCreditsConfig` |

用量卡片会在额度旁显示当前账户的邮箱（如果能够确定）：Claude Code 使用同一 OAuth 令牌查询账户资料，Codex 从当前 ChatGPT 登录的 ID Token 读取，Grok 从选中的登录条目或其令牌读取。邮箱缺失或资料查询失败时不显示；邮箱不会写入配置文件或状态栏缓存。

## 安装

### Homebrew（GitHub Release）

macOS 15 或更新版本，支持 Apple Silicon 和 Intel，无需安装 Rust：

```bash
brew install alphadoiy/tap/usage
usage
```

更新用 `brew update && brew upgrade alphadoiy/tap/usage`。如果已经安装本机版，先运行 `brew uninstall usage-local`，再安装上述版本；已有用户配置继续保留。GitHub 发布步骤见[发布与 tap 维护](docs/github-release.md)。

### 本机构建

从源码构建需要 Rust 1.88 或更新版本。

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
usage status          # 查询 OpenAI、Claude、Grok 官方服务状态，无需登录
usage status --providers openai,claude --json
usage statusline --providers claude  # 给 Claude Code 等 agent 输出轻量状态栏
usage --once --style ring    # 本次文本查询使用圆环
usage --once --style statusline  # 本次文本查询使用圆点状态栏视图
usage --providers claude,codex
usage history         # TUI 贡献看板（也可用 usage dashboard）
usage history --providers codex,claude --monthly
usage history --year 2026 --json  # 导出日/月 Token 汇总
usage providers       # 列出已注册的工具
usage projects list   # 列出项目：手动添加的目录与本机会话所在目录（--json）
usage projects add <目录>   # 手动添加项目；remove / pin / unpin 移出、置顶、取消置顶
usage codex runs --json  # 查看运行记录及启动时选中的账号、配置版本
usage resources --list   # 列出独立资源连接，不读取凭据或请求网络
usage resources --json   # 查询 New API 余额/额度与 OpenAI 官方费用
usage resources --run <运行UUID> --json # 查询启动时匹配且仍有效的资源连接
usage styles          # 列出可视化样式
usage init-config     # 写出 ~/.config/usage/config.toml
```

`usage resources` 使用独立的 `[[resources]]` 配置，支持 New API 令牌额度、账号余额和 OpenAI 官方费用。凭据通过环境变量引用，查询范围和货币单位分别表达。配置示例见[资源连接](docs/resource-connections.md)。首页的 Provider 看板展示名称、类型、用量／余额和接口模型列表，托管 Codex OAuth 账号使用自身登录查询订阅模型；查询服务通过 `profile_ids` 或独立推理连接关联，也可复用正在跟随的 CC Switch 账号中已启用的内置查询配置。运行诊断继续使用 CLI。模型查询与首页操作见[首页中枢](docs/home-overview.md)。

托管运行可以保存启动侧解析的模型、端点与资源关联，通过 `usage codex runs --json` 查看。`resources --run` 会核对启动时的连接版本和凭据，配置方法与解析边界见[运行资源关联](docs/run-resource-binding.md)。

`--once` 和 `--json` 会并发查询所选工具，输出仍按 `providers` 的配置顺序排列。

TUI 保留 `F1` 首页、`F2` 历史、`F3` 会话、`F4` Provider 管理。各页按 `?` 打开当前页面的操作菜单，直接输入名称或按键提示搜索，用方向键选择、`Enter` 执行；`Esc` 先清除搜索，再关闭菜单。不支持的操作会显示原因并禁止执行。页脚按终端宽度保留完整的常用提示，其余操作（筛选、排序、恢复方式、复制、批量清理等）均可从菜单执行。搜索和表单输入中 `?` 作为普通文本。

常用按键：

| 键 | 作用 |
| --- | --- |
| `F1` / `F2` / `F3` / `F4` | 首页 / 历史 / 会话 / Provider 管理 |
| `?` | 当前页面操作菜单；方向键选择，`Enter` 执行，`Esc` 关闭 |
| `r` | 刷新 |
| `x` | 查询首页当前 Provider 的主动重置次数，确认后重置其 5 小时额度 |
| `v` | Provider 看板查看当前 Provider 的模型性能：平均首字、生成速度、速度下限与请求明细 |
| `h` | 打开本地 Token 贡献看板 |
| `p` | 浏览 Claude Code、Codex、Grok 的本机会话，在 Ghostty 右侧分屏恢复 |
| `m` / `F10` | 打开功能菜单，选择首页、历史、会话或 Provider 管理；`m` 用于首页 |
| `F4` | 管理 Provider 账号及 Agent 配置、导入 CC Switch、打开独立实例 |
| `Tab` / `Shift+Tab` | 正向／反向切换首页看板、会话列表或历史来源筛选 |
| `j` / `k` / `↑` / `↓` | Provider 看板滚动当前 Provider 的信息；其他看板选择厂商／项目／会话 |
| `←` / `→` | Provider 看板切换不同 Provider；项目与会话看板内切换列表 |
| `Enter` | Provider 看板展开／收起模型；厂商看板查看事件与历史；项目看板进入列表并恢复会话 |
| `Esc` | 逐层返回：先清除搜索／勾选，再返回列表或上一页；首页收起详情，不直接退出程序 |
| `/` / `c` | 在项目与会话看板搜索／清除搜索 |
| `R` | 切换会话恢复方式：自动、Ghostty 分屏、当前终端 |
| `q` | 首页退出；子页面保留原有返回行为。`Ctrl+C` 随时退出 |

没有配置文件也能直接运行。默认首页展示 Provider、厂商状态、项目与会话三个看板，每 60 秒刷新一次。`style` 配置用于 `--once` 的文本用量查询，默认使用 Braille 进度条；首页不再提供无视觉效果的样式切换与保存快捷键。文本查询中，Claude 使用暖色、Codex 使用蓝色、Grok 使用黑白。Claude 和 Codex 的用量达到 70% 时变橙色、90% 时变红色；Grok 始终保持黑白。旧配置中的 `style = "both"` 读取时会转成进度条。

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

首页 Provider 定时在后台更新用量与余额，刷新期间保留现有内容；连接未变化时复用模型列表。首次加载或推理连接变化后查询模型，按 `r` 可强制重新查询。返回首页、左右切换 Provider 或上下滚动不会重复请求未变化的模型列表。余额查询失败后显示本次错误，避免继续把旧金额当作当前余额。

首页 Provider 的性能摘要展示近 24 小时已索引请求的平均首字、生成速度与速度下限，分别标注样本数。托管 BYOK 新会话可通过[本地观测网关](docs/gateway.md)自动采集 HTTP/SSE 请求；官方订阅（OAuth）始终直连，性能取自 Codex 会话日志中逐请求的结束记录与回答开始时间；默认通过 426 引导 Codex 使用 HTTP/SSE；单次启动设置 `USAGE_GATEWAY_TRANSPORT=ws` 可保留 WS 并采集逐请求首字和 TPS。历史日志继续使用 Magpie 本地日志口径。摘要按绑定缓存 5 分钟，切换 Provider 复用缓存；首页仅查询已有索引，按 `v` 更新日志并查看模型详情。

官方 OAuth 订阅支持[模型指纹测试](docs/fingerprint.md)：`usage codex fingerprint run <账号>` 采集固定 `gpt-6.1-sol` 的 3 条样本并本地评分，`enable <账号>` 在 Usage 打开期间每 5 小时运行（退出即停止），`status <账号>` 查看结果，`disable <账号>` 停用。结果仅为参考库相对排名。

Provider 看板按 `v` 查看按模型统计的请求性能，支持 24 小时／7 天／30 天及分页明细。托管 Codex 使用绑定版本的网关记录及本地日志；共享日志只索引一次，通过已确认的运行、会话、时间范围与轮次归属到账号，缺少证据时显示未归属。独立 API Provider 可显式绑定日志实例。首字仅使用明确计时，生成速度排除首字等待，保守速度下限以 `≥` 单独展示；仅弹窗打开期间后台增量索引，文件未变化时不读正文。配置、口径与性能边界见[Provider 性能](docs/performance.md)。

首页项目标题只显示名称与会话数量，不显示未加载的 Token 统计。托管 Codex 可通过生命周期 hooks 确认每次运行对应的会话，并在会话旁展示执行／等待输入等状态；首次使用需在 Codex `/hooks` 中信任 Usage 回调。活动状态在本机后台更新，不额外查询模型或余额。`usage codex runs --json` 查看确认过的会话，`usage codex run-events <运行UUID> --json` 查看不含对话内容的事件，详见[运行生命周期](docs/run-lifecycle.md)。

首页官方 API 看板以状态符号和文字独立列出 OpenAI、Claude、xAI/Grok。绿色表示官方正常，黄色表示降级，蓝色表示维护，红色表示故障；按 Enter 查看事件与最近查询历史。状态查询独立于额度和登录凭据，随首页自动刷新和 `r` 刷新，每个来源最多等待 12 秒。网络失败后保留上次结果及查询时间，标记「更新失败」；首次失败显示「查询失败」，不会当成服务正常。

数据直接来自 [OpenAI 官方 JSON](https://status.openai.com/api/v2/summary.json)（另读 `api/v2/incidents.json` 过滤已解决事件）、[Claude 官方 JSON](https://status.claude.com/api/v2/summary.json) 和 [xAI 官方 RSS](https://status.x.ai/feed.xml)。Grok 显示「未报告故障」表示 RSS 内没有未解决事件，不代表主动探测成功；OpenAI / Claude 是供应商整体状态，可能涉及你未使用的产品，也不保证单个账户的可用性。完整受影响组件和故障事件可用 `usage status` 或 `usage status --json` 查看；此命令不读取用量配置或登录文件，默认查询三个来源，`--providers` 支持 `openai` / `codex`、`claude` / `anthropic`、`grok` / `xai`。查询失败退出码为 1；供应商报告故障但查询成功时为 0。

如需浏览更多 AI 服务，可参考 [aistatus.page](https://aistatus.page/)（OpenAI、Anthropic、Gemini、xAI、Cursor、Perplexity、Midjourney 的官方源聚合）或 [LLM Status](https://llm.status.compare/)；本程序直接读取上述官方源。

Codex OAuth 的用量响应若提供主动重置次数，首页 Provider 看板会显示剩余次数（包括 0）；未返回次数时提示按 `x` 查询，不把未知显示为 0。按 `x` 查询首页当前选中 Provider 的最新次数和最近到期时间，同时更新其已加载的看板次数。查询与确认都绑定该 Provider 的独立账号目录，不回退到默认登录或 Keychain。确认框显示 Provider 名称与账号，按 `y` 才会消耗一次，按 `n` 或 `Esc` 取消；打开确认后目标固定。提交前会再次确认有可用次数，成功后刷新额度。若提交请求中断或服务端暂时不可用，结果可能无法判定；先刷新额度和剩余次数，再决定是否重试。BYOK、Claude Code 和 Grok 没有主动重置入口。

按 `p` 或 `F3` 打开会话浏览器。第一级按最近活动时间列出项目，第二级显示该项目的会话，并按 Claude Code、Codex、Grok 分组。宽终端左右排列，窄终端上下排列。用方向键或 `j`/`k` 选择项目，按 `Enter` 进入会话列表；按 `Tab` 或左右方向键切换列表。选中会话后，底部会预览工具、项目目录和即将运行的恢复命令；再按 `Enter` 恢复会话。默认 `auto` 模式在 macOS Ghostty 中打开右侧 split，在其它终端中暂时退出 TUI、使用当前终端运行 agent；agent 退出后返回原来的列表，`Ctrl+C` 中断 agent 后也能返回。按大写 `R` 轮换自动、Ghostty 分屏、当前终端；按 `y` 复制包含项目目录、实例配置目录和会话 ID 的完整恢复命令。按 `/` 搜索项目路径、标题、工具或会话 ID，按 `c` 清除搜索，按 `r` 重新读取。`Esc` 从会话返回项目，再按一次返回用量面板。新 split 会沿用对应工具的配置目录。Ghostty 分屏要求 macOS Ghostty 1.3+；当前终端恢复可用于其它终端，两种方式都需要对应工具的 CLI。Codex 优先读取本机 SQLite 会话索引，遵循用户配置的 `sqlite_home` 和 `CODEX_SQLITE_HOME`；托管账号共用该索引和会话目录。旧会话尚未进入索引时会补充扫描日志，从未运行过的目录视为没有会话。选中共享存储中的 Codex 会话后可按 `Shift+P` 选择已保存的 Provider 继续同一会话；独立来源目录会说明尚未共享，并保留默认恢复方式。恢复与复制命令沿用当前 Usage 配置文件。首次使用 Ghostty 分屏时 macOS 可能要求允许终端控制 Ghostty。列表只收录 ID 合法且本机项目目录仍存在的会话；实际恢复结果由对应工具的 CLI 决定。

会话扫描在后台运行，刷新时保留当前项目和会话选择。标题优先使用真实的首条用户提示，跳过工具结果和自动注入的上下文；Grok 子代理不会作为独立会话列出，分叉会话仍可选择。Codex 日志回退只扫描 `sessions/`，并排除在 `archived_sessions/` 中也能找到的会话；Codex 应用压缩后的 `.jsonl.zst` 日志同样可读，压缩过程中同目录存在明文副本时优先读取明文。新增会话来源可向 `SessionRegistry` 注册 `SessionSource`，用 `SessionHomes::with` 指定目录，再通过 `ui::run_with_sessions` 注入；来源的名称、颜色、排序、恢复命令和环境变量由 `AgentKind::new` 一起定义，浏览器无需增加工具分支。

## Provider 管理与 Codex 多开

运行 `usage`，在主界面按 `m` 打开功能菜单，选择「Provider 管理」并按 `Enter`；任何主面板页面都可按 `F10` 打开菜单，也可按 `F4` 直接进入。菜单可用方向键、`j` / `k` 或 `Tab` 选择功能，按 `Esc` 返回。仍保留 `usage codex` 命令入口。首页 Provider 看板的 `a`、管理页的 `a` 和 `F5` 共用同一个添加流程，功能菜单不再单列“添加 Provider”。账号凭据与 Agent 配置继续分别保存；同一订阅账号绑定的多个模型配置共享额度。每个账号配置保存名称、API Key、Base URL、模型和完整 Codex TOML；支持复制、编辑和导出。`a` 或 `F5` 打开统一添加弹窗（BYOK / OAuth 登录 / 导入 OAuth / CC Switch / WorkBuddy / Trae / ZCode / 高级接入），`l` 重新登录选中的 OAuth 账号，`e` 编辑 Agent 配置，`E` 用 `$VISUAL` / `$EDITOR` 编辑完整 TOML，`Enter` 先选择项目再启动，`o` 只更改默认项目。项目列表合并独立的项目库与本机会话所在目录：置顶项在前，其余按会话活动、从 Usage 启动的时间排序；输入可筛选，也可输入 `/` 或 `~` 开头的完整路径。选择框中 `Ctrl+A` 添加输入的路径（或选中项）、`Ctrl+T` 置顶、`Ctrl+D` 移出列表（会话保留）；在未收录的目录启动后自动加入项目库。从首页项目看板进入 F4 时，默认项目为看板中选中的项目。表单用 `Tab` / 方向键切换字段，`Ctrl+U` 清空当前字段，`Enter` 或 `Ctrl+S` 保存，密钥字段始终遮罩。模型和推理强度在字段上按 `Enter` 或直接输入打开选择列表：当前值排在首位，模型列表沿用首页的查询（已保存的 Provider 打开表单即获取；新 BYOK 填写 API Key 后按当前 Base URL 获取），输入可筛选，没有匹配项时 `Enter` 使用输入的自定义值，`Ctrl+R` 重新获取模型列表。

添加时会检测相同配置与凭据，重复项提示使用已有 Provider；不同模型或配置可以分别保存，显式“复制”仍可创建独立副本。插件账号再次登录同一身份会更新原账号凭据并保留绑定，不按昵称合并不同账号。

在 Ghostty 中启动会打开右侧分屏，可继续选择其他账号，也可重复打开同一个 BYOK / OAuth 账号。在其它终端中使用当前终端运行 Codex；也可在多个终端分别执行 `run`。需要安装 Codex CLI，使用支持 `--no-daemon` 的版本时会自动启用该参数。要让并发任务使用各自的 Git 工作目录，可传 `--worktree`（需要 Codex CLI 支持）。

```bash
usage codex
usage codex add work --base-url https://example.com/v1 --model your-model --key-stdin
# 输入 API Key 后按 Ctrl+D；也可用 --key-env 变量名 或 --key-file 文件路径
usage codex list
usage codex run work --cwd ~/Coding/project
usage codex run work --cwd ~/Coding/project --split
usage codex run work --cwd ~/Coding/project --worktree
usage codex run work --cwd ~/Coding/project -- --sandbox workspace-write
usage codex edit work --model another-model --effort high
usage codex edit work                         # 用编辑器修改完整 TOML
usage codex copy work personal
usage codex export work --output ./codex.toml  # 不含 API Key
usage codex test work                         # 仅检查配置，不发请求
usage codex test work --request               # 发送小型 Responses 请求，可能计费
```

BYOK 每次编辑或源配置更新生成新版本，账号凭据与连接配置各自保存。启动时自动继承普通 Codex 使用的用户目录（`CODEX_HOME`，未设置时为 `~/.codex`）中的全局指令、用户 skills、rules、子代理、MCP、插件和常用配置；`~/.agents/skills` 与项目上下文仍由 Codex 原生读取。全局默认 profile 的偏好也会继承，相对配置路径按原目录解析。账号明确保存的配置覆盖全局默认值，原有实例文件保留。必要时可用 `USAGE_CODEX_BASE_HOME` 显式指定继承来源。

Codex 可以正常保存模型、推理强度、通知、信任设置和其他偏好，不会再因普通偏好变化被判定为“配置被修改”。启动时将新增全局配置与实例中保存的偏好合并；已经在实例内改过或删除的设置保留，不回写全局 `config.toml`。只有供应商路由和认证配置变化需要通过 Usage 保存新版本，错误会指出发生变化的字段。

所有账号与默认 Codex 共用 `sessions`、`archived_sessions`、SQLite 会话索引和原生线程写锁。同一会话在 Usage 中只显示和统计一次；首页项目列表或 F3 会话页按 `Shift+P` 选择本次使用的 Provider，也可运行 `usage codex run <目标账号> --resume <会话UUID>`，可选 `--revision N`。共享后的普通 `Enter` 使用默认 Codex 配置；显式选择 Provider 时使用目标账号的连接和凭据。普通 BYOK 账号使用相同的原生 `usage_byok` 标识，在同一项目的 Codex 恢复列表中可以互相看到会话。Codex 自带列表仍按原生 provider 标识筛选；自定义标识不同或 OAuth 与 BYOK 之间恢复时，使用上述 Usage 入口显式选择目标账号。

原生恢复列表不显示尚无用户消息的空会话。普通恢复沿用 Codex 原生解析的有效配置，保留受信项目的模型和推理强度设置；项目文件中的 Provider 路由仍按原生规则忽略。

启动先检查项目目录、参数和 CLI 支持，再合并空闲旧实例的会话目录，并保留旧路径的访问方式；不改写 Codex 的数据库结构。运行中的旧实例暂缓迁移，关闭后重试；遇到同名文件冲突或跨磁盘迁移时保留原文件并报错。账号的 `auth.json` 和 API Key 不共享。

数据默认位于 `~/.local/share/usage/codex`，遵循 `XDG_DATA_HOME`；`USAGE_CODEX_DIR` 可指定一个专用目录。BYOK API Key 保存在该目录下的 `keys/<UUID>` 文件，SQLite 只保存密钥引用；macOS/Linux 的数据与密钥目录权限为 `700`，密钥文件权限为 `600`。文件内容为明文，权限控制不等于加密。BYOK 管理不访问 macOS Keychain，生成的 TOML 通过子进程环境变量取 Key。启动命令和复制的恢复命令不包含密钥，也会清除继承的默认 Codex/OpenAI 认证环境变量。移出列表会保留配置版本、密钥和历史，便于恢复旧会话。

如果此前保存过旧版 Keychain 配置，可编辑账号重新输入 API Key；从 CC Switch 导入的配置也可重新导入，源内容一致时会修复原配置的密钥引用，保留版本和会话目录。不会访问或删除以前的钥匙串条目。已有的官方订阅额度查询仍可读取 Claude/Codex 自己的 Keychain 登录凭据。

### ChatGPT OAuth 账号

在账号管理页按 `a`，选择「OAuth · 登录 ChatGPT」，直接选择浏览器登录或设备码登录，授权成功后自动使用 OAuth 凭据中的邮箱作为账号名称。也可以选择「导入 OAuth」并指定官方 `auth.json` 或 Cockpit Tools 的单账号 JSON；界面只展示邮箱、套餐和账号 ID，预览后按 `Enter` 保存。列表显示账号的 OAuth / BYOK 类型及 OAuth 身份；按 `l` 重新登录，登录到另一个用户或工作区时会拒绝替换原账号。

CLI 与账号管理页共用认证注册表和登录任务，显示准备、等待授权、保存及最终结果；原生 CLI 直接展示授权链接或设备码。授权过程中按 `Ctrl+C` 取消，CLI 退出码为 `130`，账号管理页返回列表。保存开始后等待提交结果，避免中途取消造成账号状态不明。准备阶段的 CLI 版本检查最多等待 10 秒，取消会在检查返回后生效。

```bash
usage codex login personal
usage codex login work --device-auth
usage codex login --account personal             # 重新认证，保留名称和配置
usage codex import-auth --source ~/.codex/auth.json --name personal
usage codex import-auth --source /path/to/cockpit/account.json --name work
usage codex login personal --config ./oauth.toml # 可选完整配置
usage codex edit personal --model your-model --effort high
usage codex run personal --cwd ~/Coding/project
```

登录和 refresh token 刷新交给官方 Codex CLI，不自行实现 OAuth 端点或在应用中硬编码 client ID。[官方认证文档](https://learn.chatgpt.com/docs/auth)说明 CLI 支持浏览器 / 设备码登录，以及文件凭据存储。设备码授权需要个人或工作区启用该功能。新增账号默认使用 CLI 的默认模型，也可选择账号可用的模型。

OAuth 每个账号使用一个固定 `CODEX_HOME`，登录保存在其中的 `auth.json`；目录权限 `700`，凭据文件权限 `600`，明文存储，不接入 Keychain。SQLite 保存身份信息和配置，令牌不进入数据库、导出的 TOML、账号列表或启动命令。配置版本使用 `usage-rN.config.toml`，通过原生 `--profile` 选择；需要 **Codex CLI 0.134.0 或更新版本**（[原生 profile 文档](https://learn.chatgpt.com/docs/config-file/config-advanced#profiles)）。修改配置不会复制或还原旧 token，CLI 刷新后继续使用最新凭据。在共享会话列表中可选择该 OAuth 账号继续任意会话，默认使用该账号当前配置；可用 `run --revision N --resume UUID` 显式选择旧配置，仍使用最新登录。

不同 OAuth 账号、同一 OAuth 账号都可以并行运行。同账号实例共用一个 `CODEX_HOME` 和 `auth.json`，通过原生 profile 选择各自配置版本，token 刷新及凭据重新加载由 Codex CLI 处理。macOS/Linux 的运行实例及其仍存活的子任务持有共享锁，不限制多开；即使 `usage` 启动器异常退出，也不会提前释放运行锁。重新登录和导入替换凭据需要独占锁，必须先关闭该账号的所有托管实例和子任务。运行中允许保存新的配置版本。OAuth 不能复制为另一个账号，以免复制 refresh token。重新登录失败、取消或身份不符时，原账号登录保留。登录暂存目录在流程结束后清理。

导入只读取指定文件，是一次性登录快照，不与 Cockpit / 默认 Codex 目录双向同步，也不读取系统 Keychain。相同用户和工作区重复导入会更新已有账号，不同工作区分别保存；运行中的账号拒绝导入更新。已有 refresh token 若仍被来源工具使用，来源刷新后导入副本可能失效，因此长期并行使用建议为托管账号独立登录。导入要求完整 id / access / refresh token；凭据是否仍有效由官方 CLI 验证，失效时按 `l` 重新登录。`test` 可检查本地配置和登录结构，`test --request` 的 Responses API 请求仅用于 BYOK。

### Bark 会话通知（动态注入）

安装 iPhone 上的 [Bark](https://github.com/Finb/Bark)，复制 App 中的推送 URL，再配置 `usage`：

```bash
usage codex bark configure --url-stdin  # 粘贴 URL，回车后按 Ctrl+D；不会发送推送
usage codex bark status                # 只显示开关与服务器，不显示设备密钥
usage codex bark test                  # 显式发送测试推送
usage codex run personal --cwd ~/Coding/project
usage codex bark disable               # 运行中的会话也停止推送后续事件
usage codex bark enable                # 重新开启已保存配置
usage codex bark logs --tail 100       # 按时间合并多个通知进程的诊断日志
usage codex bark logs --run <运行UUID> # 只查看某个启动实例
```

也可通过 `configure --url-env BARK_URL` 从指定环境变量读取 URL。`configure --no-approval` 只提醒本轮完成，`--no-completion` 只提醒权限审批请求。Bark 配置在管理数据目录的 `bark.json` 中，权限 `600`、明文存储；通过 `USAGE_CODEX_DIR` 可以分别管理不同通知设置。账号导出和 CC Switch 同步不携带 Bark 配置。

开启后，`usage` 仅向它启动的 Codex 子进程传入临时 `-c notify=...` 和 `-c hooks.PermissionRequest=...`，并通过子进程环境传递本次运行上下文。CLI、账号管理页，以及首页和 F3 会话页在当前终端、Ghostty 或复制命令中恢复的 Codex 会话都使用通知入口。默认 Home 和其他未托管 Home 通过 `usage codex resume-local` 包装恢复，保留原来的 Home、认证环境和账号文件，不导入托管账号。不会将通知回调写进默认 `~/.codex`、托管账号配置或项目配置；直接运行 `codex`，即使使用相同托管 `CODEX_HOME`，也不会获得 `usage` 的 Bark 通知。账号原有的 `notify` 回调仍会收到原完成事件，已有 hook 来源保留。

完成提醒表示**本轮回答结束**，不保证整个需求已经实现。审批提醒表示**出现权限审批请求**，其他审批机制可能随后处理请求；通知 hook 不会批准或拒绝操作。审批使用异步 hook，需要支持生命周期 hooks 的新版 CLI（已用 0.162.1 验证）；首次在 Codex 中打开 `/hooks`，审阅并信任 `usage` 的 PermissionRequest hook。账号和配置版本的运行目录不同，可能需要分别信任；可执行文件路径或 hook 定义变化后需要重新审阅。遵循已有的 hooks 开关和管理员策略，不自动绕过信任检查。CLI 保存的合法 hook 信任状态可以保留，账号配置版本仍不变。等待普通问题回答、额度耗尽、网络卡住暂不单独检测。

自动通知仅在 **Mac 锁屏或所有在线显示器熄屏** 时发送，没有“始终通知”选项。解锁且亮屏时直接丢弃事件，不在之后锁屏时补发；读取不到状态时也不发送。判断通过本机 Core Graphics 完成，不截屏、不读取屏幕内容。多显示器中任一显示器亮着且未锁屏时不通知。当前状态可用 `bark status` 检查；`bark test` 是显式测试，会直接发送，方便在电脑前验证通知通道。这项自动状态检测目前仅支持 macOS。

完成通知标题包含项目名和“本轮完成”，正文摘录最后一条助手回复（合并空白，最多 400 个字符）并附账号名；没有回复时明确提示返回电脑查看。审批通知显示项目名、工具名和申请理由（若回调提供 `tool_input.justification`），提醒返回电脑处理。通知不展示会话 ID，也不额外读取用户提示词、原始命令或完整项目路径；回复摘录本身可能包含助手提到的代码、路径等内容。完成通知只针对用户会话：只读核验该进程实际 `CODEX_HOME` 的会话索引，必要时读取对应记录文件的首行元数据，排除标题生成等临时线程及子代理；无法核验来源时也跳过（包括不保存记录的 `--ephemeral` 会话）。审批通知保持独立。每次启动有独立分组；每次收到支持的事件后立即检查来源、屏幕状态和通知开关，符合条件就发送，不做时间窗口合并或事件去重。发送使用 JSON POST、5 秒超时，推送失败不阻止 Codex；后续回调仍可尝试推送，不做后台自动重试。

Bark 使用现有的结构化日志和轮转规则。默认日志记录原始 `thread_id` / `turn_id`、回调类型、客户端、账号、项目名、工具和子代理标识；回调进程的 PID 和进程标识由日志 span 提供。同时记录完成回复摘录（最多 200 字符）与准备发送的通知预览（最多 300 字符）。来源核验会记录查询的数据库版本、索引或记录文件的查询结果、原始 `thread_source` / `session_source`、具体失败原因；无持久记录只标记为 `untracked`，不会断言它一定是标题线程。还记录锁屏与熄屏状态、忽略原因、HTTP 状态、Bark 结果码、请求耗时及失败阶段。不额外记录配置中的设备密钥、推送 URL、完整事件 payload、用户提示词或原始工具命令。`bark logs` 按时间汇总各通知进程，可用 `--run` 筛选启动实例；通用 `usage logs --operation <运行UUID>` 也能关联该实例的通知回调。`run_id` 对应启动操作；未托管 Home 的恢复不创建托管运行记录，启动日志 `bark_local_launch` 记录是否注入、子进程 PID、Home、项目目录和会话，便于区分未注入与回调被过滤。每次回调的 `operation_id` 关联接收、核验和发送过程，原始线程和轮次 ID 可与 Codex 原生日志直接对照。保留 `event_id` 指纹以对照历史记录，并区分同一线程和轮次中不同的审批事件，不用于去重；不再重复记录 `thread_key` 或 `has_turn_id`。`USAGE_LOG=off` 时不记录。

在 iPhone 的 Watch App 中打开 Bark 的通知镜像；手机锁屏、手表佩戴且解锁时，通常由 Watch 提醒。专注模式会影响通知，Mac 需要保持唤醒、联网。详见 [Apple Watch 通知规则](https://support.apple.com/zh-cn/108369)、[Codex 单次配置覆盖及 notify](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications)、[hooks 信任说明](https://learn.chatgpt.com/docs/hooks)。

### 从 CC Switch 导入和跟随

界面按 `i`，指定数据库或目录并预览，按 `Enter` 一次性导入，或按 `f` 导入并跟随。默认读取 `~/.cc-switch/cc-switch.db`，支持自定义目录。命令行可按供应商 ID 选择导入范围：

```bash
usage codex import --dry-run
usage codex import --follow
usage codex import --source /path/to/cc-switch.db --provider vendor-id --follow
usage codex sync --dry-run
usage codex sync
usage codex sync --watch --interval 15
usage codex follow work --off
usage codex detach work
```

导入只读源 SQLite，并读取已提交的 WAL 数据，不迁移或改写 CC Switch 数据库。重复导入通过“来源路径 + 供应商 ID”识别同一账号。开启跟随后，启动前自动同步，管理界面每 15 秒同步一次；只更新已跟随的供应商，CC Switch 切换当前供应商不会改变这里选用的账号，也不会自动导入源中新增加的供应商。

本地编辑会暂停跟随，再次导入遇到本地修改会报告冲突并保留本地配置。需要同时保留本地版和源版本时，先 `detach` 解除来源绑定，再导入创建新的配置；名称重复时使用 `list` 显示的完整 ID 选择。源供应商被删除或数据库暂时不可读时，已保存的配置和历史仍然可用。

CC Switch 自动导入支持原生 Responses API 的 Codex BYOK 配置，导入供应商 TOML 和旧版显式启用的公共配置片段。CC Switch 中的官方订阅 OAuth（请通过上述登录 / 导入登录文件入口添加）、接管代理占位配置、需要 Chat Completions/Anthropic 协议转换的路由会跳过并说明原因。新版 CC Switch 仅保存在当前 Codex 配置文件中的全局设置、MCP、独立命名 profile 文件尚不自动同步；可通过 `add --config` 或 `edit --config` 导入完整 TOML 补充。暂不支持 SQL 导出文件、云同步或写回 CC Switch。

## TUI 贡献看板

使用 `F1` / `F2` / `F3` 切换首页、历史、会话。主面板按 `h`，或运行 `usage history` / `usage dashboard`，进入终端内的历史用量看板。布局按“年度概览 → 贡献图 → 选中日期与近期用量”排列：来源筛选放在顶部，摘要显示当前范围的 Token、活跃日和缓存读取占比。切换页面会保留当前日期、来源、日/月视图，以及会话选择和搜索。

贡献图参考 [hindsight](https://github.com/chaosprint/hindsight) 的前景色小方块样式，列间留空，星期只标周一/周三/周五。蓝色阶按当前年份、当前来源的活跃用量划分，使峰值很高时较小用量也容易区分；零用量显示灰色方块，未来日期留空。选中日期显示 `▣`，下方集中展示输入、输出、缓存和来源分布。窄终端只显示选中日期附近的周；宽终端将选中日期明细与近期列表并排。

近期列表最多显示 7 条，跳过未选中的零用量日期，不随终端高度铺满。日、月列表均按新到旧排列：`↑` / `k` 选择更新的日期或月份，`↓` / `j` 选择更旧的日期或月份。按 `PgUp` / `PgDn` 跳到更新/更旧的活跃日，可快速浏览稀疏历史；普通方向键仍能选中无用量的过去日期。

按 `v` 轮换全年、今天、近 7 天、近 30 天、本月；日期范围均按本地日历日期计算，含起止当天。按 `f` / `M` 打开项目 / 模型筛选弹窗，可输入文字搜索，用方向键选择并按 `Enter` 应用，`Esc` 取消。按 `i` 打开实例筛选；按 `c` 清除项目、模型、实例和来源筛选。图表、摘要和明细使用同一筛选范围；窄终端保留最多 7 条近期日期。

选中日期或月份后按 `Enter`，会话页只显示该时间范围内有匹配用量、且本机仍可恢复的会话。项目与会话显示对应范围的 Token，底部预览显示模型；按 `o` 切换最近活动 / Token 排序，`Esc` 从会话返回项目，再返回刚才的历史页。已删除或无法恢复的会话仍保留在用量合计中。普通 `F3` 会话页展示所有可恢复会话，用量合计为已保存的全部历史。

会话列表突出标题，时间与实例放在次行，Token 在空间足够时右对齐。宽终端并排显示项目与会话，窄终端只显示当前聚焦的列表，通过 Tab 或左右键切换。窗口高度足够时，选中会话会在后台读取 Codex、Claude Code 或 Grok 的最近请求与回复摘要；不展示工具输出和思考内容，不将预览写入磁盘。切换会话会取消旧预览，结果按实例与会话身份核对；无法读取或日志过大时显示说明。

会话清理参考 magpie 的删除与回收站流程。在 `F3` 会话列表按空格勾选，`a` 勾选／取消当前项目列表中的全部会话，`d` 或 Delete 删除勾选项（未勾选时删除当前会话）。项目列表按空格也可勾选该项目的会话。按 `?` 可选择“清理当前项目列表”；旧的 `A`、`D` 仍兼容。搜索和历史时间筛选继续生效，项目批量清理只包含当前筛选下列出的会话；确认窗口列出数量、标题及实例，按 `y` 执行、`n` / `Esc` 取消。搜索中 `Enter` 应用、`Esc` 清除并返回；退出输入后，`Esc` 依次清除搜索、取消勾选、返回项目列表、返回上一页。

目前支持 Codex、Claude Code 和 Grok，包括 Claude 的子代理、文件历史、环境和待办文件，Codex 的日志分段、压缩副本和子代理日志，以及 Grok 的独立会话目录和子代理目录。自定义 Agent 暂不支持清理。最近一分钟有文件写入的会话、仍在托管运行的会话会跳过；Codex 还会检查会话及子任务的原生写锁，等待输入的活动会话同样受保护。Usage 已记录的共享目录链接按真实存储解析，未知符号链接及未知文件布局会拒绝处理。批量操作逐项报告失败，不影响其他可清理会话。

删除先将文件移到 `$XDG_DATA_HOME/usage/trash/sessions`（默认 `~/.local/share/usage/trash/sessions`），不会立即释放文件所占空间。按 `b` 或通过 `?` 打开回收站，`Enter` 恢复，`d` / Delete 永久删除选中项，`e` 清空当前回收站；均需确认。`u`、`E` 保留兼容。回收站不会自动过期。恢复不会覆盖原位置已有文件；旧账号迁入共享存储后，原回收站条目与删除标记继续关联同一会话。中断的清理可重新打开回收站恢复。跨磁盘移动暂不支持，会报错并尝试恢复已移动文件。不会改写 Agent 的共享索引；永久删除后保留用于过滤残留索引的最小身份记录，Usage 刷新时不会重新列出已删除会话。

按 `m` 切换月度视图：显示选中月份的输入、输出、缓存及来源合计，与 12 个月的彩色堆叠趋势条；不再重复堆叠月度表格。蓝色为 Codex、暖色为 Claude、灰白色为 Grok。较矮终端优先显示月度趋势，较高终端同时保留热力图。

| 键 | 作用 |
| --- | --- |
| `0` / `1` / `2` / `3` | 全部 / Codex / Claude / Grok（在本次 `--providers` 范围内） |
| `F1` / `F2` / `F3` | 首页 / 历史 / 会话 |
| `Tab` | 轮换来源筛选 |
| `v` | 全年 / 今天 / 近 7 天 / 近 30 天 / 本月 |
| `f` / `M` / `i` | 搜索并选择项目 / 模型 / 实例 |
| `c` | 清除项目、模型、实例和来源筛选 |
| `Enter` | 查看选中日期或月份的会话 |
| `m` | 每日 / 月度视图 |
| `↑` / `k`、`↓` / `j` | 选择更新、更旧的日期或月份 |
| `←` / `→` | 前一周 / 后一周 |
| `PgUp` / `PgDn` | 更新 / 更旧的活跃日 |
| `[` / `]` | 上一年 / 下一年 |
| `t` | 回到今天 |
| `r` | 重新读取日志 |
| `Esc` | 返回主面板；`q` 保留原有返回／独立打开时退出的行为 |

```bash
usage history --year 2026
usage dashboard --monthly --providers codex,claude,grok
usage history --providers grok --monthly --once
usage history --json
usage history --range 7d --project usage --model gpt
usage history --from 2026-09-01 --to 2026-09-30 --monthly --json
usage history --range 30d --csv > usage.csv
usage history --refresh 0
usage history --providers codex --instance work-codex --range 7d --json
```

历史数据参考 cockpit-tools 的会话日志统计方式，读取配置中的本地目录：Codex 的 `sessions/` 和 `archived_sessions/`、Claude 的 `projects/`（包括子代理日志）、Grok 的 `sessions/**/updates.jsonl`。读取在后台进行，启动先显示已保存统计，再扫描更新；使用配置中的 `refresh_seconds` 自动刷新当前历史或会话页的本地用量，`0` 禁用自动更新。按 `r` 手动更新时保留现有内容。历史页不会因自动刷新而反复查询官方额度；连续刷新额度也会复用进行中的查询。

Codex 优先读取 `last_token_usage`，缺失时使用累计值的增量，去掉重复额度快照和分叉会话重放前缀；Claude 按消息 ID 合并流式记录；Grok 按 prompt ID 去重，只统计 `turn_completed` 的汇总。日期按本地时区归属。总 Token 为输入加输出，缓存已经包含在输入内，不会重复相加；Claude 的输入包含缓存读取和缓存写入，Codex/Grok 的缓存读取是输入的子集。Grok 的调用数来自 `modelCalls`，Codex/Claude 则为有效用量记录数，三者不能视为严格一致的 HTTP 请求次数。

已解析的用量、模型、项目路径、会话 ID 和文件游标保存到 `~/.local/share/usage/history.sqlite3`（设置 `XDG_DATA_HOME` 时使用该目录下的 `usage/history.sqlite3`）。日志追加时只读取新增完整行；未写完的行留到下一次更新，文件截断或指纹变化时重建该文件的统计。账本写入使用事务，删除原日志后仍保留之前已统计的历史；在首次统计前已删除或从未记录的历史无法还原。缓存不保存对话内容或登录凭据：文件边界只保存 SHA-256 摘要，旧版保存的原始边界字节会在下次读取时就地换成摘要并压缩数据库；无法解析的单条记录会被跳过，不影响其余历史。

它与主面板的官方滚动额度百分比是不同的数据。`--json` 输出所选范围的日/月汇总（包括零用量日期）、实例分项、最近同步时间、扫描文件数、新读取字节数和读取警告，不包含对话内容、登录凭据或项目路径。`--project` / `--model` 按不区分大小写的包含关系筛选；日志中未记录模型时不会猜测模型名称。`--csv` 输出日期、总 Token、输入、输出、缓存读取和记录数，配合 `--monthly` 可导出月度汇总。

## 额度提醒、额外实例与恢复方式

这些配置均可选；原有配置可以继续使用。额度提醒默认关闭，在 TUI 持续运行时检查成功的官方额度快照：剩余额度首次达到阈值或从阈值上方降到阈值内时提醒，连续低额度不会重复提醒；恢复到阈值上方时可提示恢复。冷却期内再次达到低额度阈值时暂缓提醒，冷却结束后的下一次有效查询若仍处于低额度则补发一次；额度提前恢复会取消待提醒状态。冷却时间按工具、当前账号和额度窗口分别计算，仅在本次运行中保存；失败、过期、网络抖动和过时响应不会触发恢复通知，也不会取消待提醒状态。没有账号标识时按工具和窗口区分。提醒显示在顶部，可选择终端铃声或本机系统通知。

```toml
[alerts]
enabled = true
remaining_percent = 15
cooldown_seconds = 300
recovery = true
bell = false
desktop = false

[sessions]
resume = "auto" # auto / ghostty / current

[[instances]]
id = "work-codex"
provider = "codex"
path = "~/.codex-work"

[[instances]]
id = "work-claude"
provider = "claude"
path = "~/.claude-work"
```

额外实例用于历史统计和会话恢复，支持 Codex、Claude、Grok。默认目录的实例 ID 为 `codex`、`claude`、`grok`，额外 ID 必须唯一；同一来源不能配置重复或嵌套目录。读取原始日志时不改写这些目录；恢复会话时使用选中实例的配置目录。不同实例中的同 ID 会话分别统计，复制同一会话到两个实例会在全部实例合计中分别计入，可用 `--instance` 或历史页的 `i` 限定范围。旧账本无需重建，原来的记录自动归入对应默认实例。官方额度和提醒仍使用默认目录的当前登录身份，额外目录不会自动切换账号。

系统通知在 macOS 使用原生通知；其它系统尝试 `notify-send`。复制命令在 macOS 使用 `pbcopy`、Windows 使用 `clip.exe`、Linux 尝试 `wl-copy` / `xclip` / `xsel`，缺少剪贴板工具时会给出提示。

## 状态栏

`usage --once --style statusline` 使用圆点文本视图，适用于所有已注册工具。可在配置文件中设置 `style = "statusline"` 作为文本查询的默认样式。例如 `5小时  ● ● ● ● ● ○ ○ ○  57%  ⟳ 2h0m`。

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

## 排查日志

使用 `tracing`、`tracing-subscriber` 和 `tracing-appender` 写入 JSONL 文件日志。默认目录为 `~/.local/state/usage/logs`，设置 `XDG_STATE_HOME` 时使用 `$XDG_STATE_HOME/usage/logs`；可用 `USAGE_LOG_DIR` 单独覆盖。普通命令默认 `info`，轻量 `statusline` 默认 `warn`，没有事件时不创建日志文件。

```bash
usage logs --path                          # 查看目录
usage logs --tail 100                      # 最新日志文件的最后 100 条事件
usage logs --operation UUID --tail 1000    # 在保留文件中查找一次操作
USAGE_LOG=debug usage codex run ACCOUNT --cwd /path/to/project
USAGE_LOG=off usage codex list             # 关闭日志
```

Codex 账号保存、CC Switch 导入/跟随同步、配置校验、OAuth 凭据读写及共享/独占锁、原生登录、子进程启动/退出、Responses 测试和官方额度查询都有事件。每个操作包含操作 ID、进程 ID、阶段与耗时；账号链路另含内部账号 UUID、配置版本和认证方式。启动成功后，操作 ID 同时作为 SQLite `runs.id`，启动前失败也有操作 ID。CLI 错误会在 stderr 提示日志目录和入口操作 ID，用它能查到嵌套阶段。

配置不一致会记录 `config_mismatch`、安全的字段名（例如 `model`、`model_providers.<entry>.base_url`），不记录配置值。日志不包含 API Key、OAuth token、邮箱、账号名称、完整配置、启动参数或 HTTP 请求/响应内容；第三方库事件即使在 `trace` 级别也被过滤。原生 Codex 的终端输出仍由它自己输出，不复制进文件日志。

每次启动独立写文件，按 UTC 日期或约 5 MiB 轮换，单条事件保持完整。新目录权限 `700`，文件 `600`，已有父目录权限保持原样。初始化和轮换时清理超过 7 天的日志，并将保留量控制在约 50 MiB / 200 个文件；运行中锁定的文件不会被清理，因此多开时上限可能暂时超过。写入使用有界后台队列，队列满时回压而不主动丢弃事件，正常退出会刷新队列；强制终止可能丢失尚未写入的事件。

`--help`、`usage logs` 不初始化日志；日志初始化失败会提示并继续执行原命令。详细阶段开始/结束在 `debug` 级别，失败阶段在默认级别也会保留。`USAGE_LOG` 支持级别或 tracing 过滤规则；规则无效时回退到 `info`，不回显原始规则。

## 扩展

Agent 和资源服务分别通过工厂注册表扩展，再由 `hub::Hub` 组合成配置实例。Agent 提供启动与会话能力，资源适配器提供额度、余额或支出快照。首页通过独立应用服务读取 Provider 目录、模型列表与类型化用量／余额；运行与资源核对读模型、资源 CLI 和原有订阅查询保持独立。完整结构和迁移边界见 [Provider 架构](docs/provider-architecture.md)。

新增 Agent 实现 `AgentAdapter`，注册到 `AgentRegistry`；新增资源服务实现 `ResourceReader`，注册到 `ResourceRegistry`。注册 ID 重复会返回错误，不再静默覆盖。

现有额度视图仍支持旧的 `UsageProvider` 扩展方式，在 `builtin_factory()` 里登记工厂。多账号扩展应覆盖 `cache_key()`，为每个账号和配置版本提供独立标识。

```rust
factory.register("gemini", |ctx| Box::new(GeminiProvider::new(ctx)))?;
```

工具若支持主动重置，可另外实现 `UsageReset`，并让其 `UsageProvider::reset()` 返回该能力；默认提供方不提供重置。

新增一种画法：向 `StyleRegistry` 登记一个函数。它把快照中的额度窗口编排为一种视图；内置的进度条使用 Braille 点阵，圆环使用 `Canvas`。

```rust
registry.register("spark", "另一种", render_spark);
```

注册之后，`--once --style` 和配置文件都能选择这种文本用量视图。

Provider 插件接入（WorkBuddy / Trae 国内版与国际版 / ZCode）：见 [插件文档](docs/plugins.md)。

原生 Codex 上下文兼容性可用 `python3 tests/support/native_codex_context.py target/debug/usage /path/to/codex` 验证。该检查使用临时用户目录和合成凭据，对比直接启动与托管启动的 skills，验证 A 的会话出现在 B 的原生 `/resume` 列表、恢复后使用 B 的路由和凭据，以及跨 provider 恢复与偏好保存；对话请求由本地模拟服务响应，不调用真实模型服务。
