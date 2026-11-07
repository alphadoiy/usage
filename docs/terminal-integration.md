# 终端适配规范

终端是 Agent 启动命令的传输目标，与 Provider、Agent 类型、账号凭据和会话来源分离。实现集中在 `src/terminals/`。首页、会话恢复和 Provider 管理使用相同入口；`sessions` 中的旧函数只做兼容委托。

## 当前行为

调用方只表达意图（`layout`、`focus`），由终端适配器声明自己能做什么（分屏、标签页、是否按现有 pane 尺寸自适应），`resolve` 只解析能力层面的布局偏好；读取实时拓扑后，`layout::decide` 的 `Action` 决定唯一实际落点。能力层面：`auto` 与 `split` 优先分屏，`tab` 优先标签页；终端做不到时降级到另一种，不会换终端。界面文案由 `TerminalTarget::describe` 生成，会如实标注降级，例如仅支持标签页的适配器会标注“不支持分屏”。

| 目标 | 检测上下文 | 能力 | 执行方式 |
| --- | --- | --- | --- |
| Herdr 原生 App | 非空 `HC_TERMINAL_ID`、`HC_TERMINAL_SOCKET`；优先 `HERDR_APP_HOME/bin/herdr-app` | 分屏、标签页、**自适应** | `herdr-app ls` 读取 space/tab/pane，再 `pane.split` 或 `tab.new`；启动后按配置切回 Usage 的 pane/tab 或聚焦新 pane |
| Rex | 非空 `REX_SESSION`；通过 `REX_BLOCK` 定位调用方，并读取 `REX_BIN_DIR`、`REX_SERVER` | 分屏、标签页、**自适应** | `rex session inspect --json` 读取窗口/block，再 `rex run --session … --cwd … --focus=<bool> --keep-open --json [--block … --split … --ratio 50] -- sh -lc COMMAND` |
| Ghostty | macOS，且有 `GHOSTTY_RESOURCES_DIR` 或 `TERM_PROGRAM=ghostty` | 分屏、标签页、**自适应** | 先用 AppleScript 按 Usage 进程的 TTY 找到 anchor terminal，再按决策指定的目标 terminal 用 `split … direction right/down` 或 `new tab in w`；`focus` 明确切回 anchor 或聚焦新 terminal |
| 当前终端 | 显式选择，或自动检测未命中 | — | 调用方退出 TUI，委托原 Agent Launcher，退出后返回 TUI |

## 布局与焦点配置

```toml
[sessions]
resume = "auto"  # auto / ghostty / rex / herdr / current
layout = "auto"  # auto / split / tab
focus  = "new"   # new / keep
```

- `layout = "auto"`：能分屏就分屏；自适应终端（目前是 Ghostty、Herdr 和 Rex）按下面的规则自动排布。
- `layout = "split"`：始终在 Usage 所在标签页分屏，自适应终端忽略每页上限，必要时才低于最小尺寸。
- `layout = "tab"`：始终新建标签页。
- `focus = "new"`（默认）：切到新实例；显式 `keep` 留在 Usage，方便连续创建多个实例。Herdr 创建成功后若焦点操作失败，会明确提示“实例已创建”，避免误以为需要重新启动。

两项都可省略，未知取值和显式配置文件读取失败会直接报错，不静默回退。`usage codex run --split` 的含义仍是“在新终端运行”，具体落点由上述配置决定。

## 自动排布（自适应终端）

决策是 `src/terminals/layout.rs` 中与终端无关的纯函数，输入为适配器读取的拓扑（标签页、pane 的行列数、Usage 所在 pane），输出为“切哪个 pane、朝哪个方向”或“新标签页”。Rex、Herdr 与 Ghostty 可列出 pane 但不提供可靠尺寸，`src/terminals/adaptive.rs` 会以 Usage 自身 pane 的尺寸为起点，把 Usage 创建的 pane 尺寸按对半切记录到 `~/.local/share/usage/layout/`，下次启动时与实时 pane 列表合并。已消失的 pane 自动遗忘；其它程序创建的 pane 计入数量但不会作为切分目标。状态写入是原子且 best-effort，失败只记 debug 日志，不影响启动。 Usage 尺寸或 pane 列表与上次记录不一致时，历史尺寸估算失效；无法读取尺寸时不假定大窗口。其他 pane 的手动缩放若没有改变 Usage 尺寸或列表，当前接口无法检测，因此最小尺寸是基于估算的目标，不是严格保证。

1. 优先使用同一项目的溢出标签页，其次是 Usage 所在标签页；已有 4 个 pane 的标签页视为已满。
2. 在候选标签页里选面积最大的 pane 对半切；面积相同时避开 Usage，保持仪表盘完整。
3. 方向按像素比例判断（字符格高约为宽的两倍）：宽于高则左右切，否则上下切；首选方向放不下时尝试另一方向。
4. 按可用尺寸估算，切后每一半不能小于 70 列 × 16 行，否则该 pane 不参与；没有可切的 pane 就新开标签页。
5. 新标签页以项目目录名命名，并在本地布局状态中记录项目标识；同一项目的后续实例继续填充它，其它项目不会混入。显式 `tab` 创建的标签页不记录标识，不参与填充。

200×50 的窗口依次启动实例时，Usage 与前三个实例排成网格，之后的实例进入该项目的溢出标签页，同样平铺。

自动检测按注册顺序执行：Herdr 优先于 Rex，Rex 优先于 Ghostty。Herdr 先于 Ghostty 是因为原生 App 的 `TERM_PROGRAM` 可能就是 `ghostty`；Rex 先于 Ghostty 是为了避免残留的 Ghostty 标记抢占 Rex。Herdr 与 Rex 互相嵌套时，环境变量无法判断哪个是最内层，Herdr 先于 Rex 只是固定的启发式顺序；需要使用另一个终端时，请在配置里显式设置 `resume = "rex"`（或 `herdr`）。仅安装某个终端不代表当前运行在其中。显式选择不可用终端时报错，不改为其它终端，也不在失败后再次启动 Agent。

## 结构与设计模式

```text
Agent Launcher / ResumePlan
    │ 导出包含标识符的恢复或启动命令
    ▼
TerminalRequest { cwd, shell_command, options }
    ▼
TerminalRegistry + Selection + TerminalContext
    │ 选择策略、校验 cwd、绑定环境快照
    ▼
TerminalBackend（Adapter / Strategy）
    │ 构造控制命令及可选的后续命令
    ▼
TerminalLaunch（Command）
    │ 统一执行与失败处理
    ▼
终端自己的 PTY → Agent → 原生终端协议
```

- **Adapter / Strategy**：`TerminalBackend` 封装工具特有的检测和启动命令。Herdr、Rex CLI 与 Ghostty AppleScript 在独立文件中，不进入业务层。
- **Registry**：`TerminalRegistry` 管理有序策略，拒绝重复、空或保留 ID。内置注册表初始化后只读；测试可注入独立注册表，无需修改进程全局环境。
- **Command**：`prepare` 返回 `TerminalLaunch`，构造阶段不启动终端；`execute` 才运行控制程序。执行失败直接返回，避免重复启动。
- **依赖倒置**：UI 只使用目标描述与 `is_current()` 判断是否需要交还当前终端，不根据 Herdr、Rex、Ghostty 等具体名称分支。配置字符串到适配器 ID 的映射集中于 `ResumeMode::backend_id`。

这里使用轻量对象注册表，不引入动态插件加载、额外继承层次或重复的工厂体系。

## 接口约定

| 接口 | 职责 |
| --- | --- |
| `target()` | 返回稳定 ID、界面标签和能力 `Capabilities { split, tab, adaptive }` |
| `environment_keys()` | 声明检测及控制 CLI 需要的环境变量；注册表只采集这些变量 |
| `available(context)` | 只读检查平台和活动上下文；不启动进程、不访问文件系统、不探测网络、不创建窗口 |
| `command(context, request)` | 根据已校验请求构造 `TerminalCommand`（控制命令及可选的后续阶段，每个阶段可再链接下一阶段）；不执行命令，不再次读取全局环境；允许对上下文中的 PATH 做只读文件探测以定位控制 CLI |
| `registry.prepare(...)` | 选择策略，检查可用性、cwd 与空命令，将声明的环境变量绑定到计划 |
| `launch.execute()` | 启动控制 CLI，检查退出码并报告失败；不更换后端重试 |

`TerminalRequest.shell_command` 已由 Agent 层完成转义，终端适配器必须视作不透明字符串。支持 argv 的 CLI 用一个独立参数携带它；通过 shell 执行时显式传入 `sh -lc`，不可拼接进外层 shell 命令。cwd 同样用独立参数传递。

凭据只能由 Agent 启动层读取或注入。终端请求、控制 CLI 参数、普通日志中只能包含 Usage 路径、配置路径、账号 ID、会话 ID 等标识符。上下文、启动请求和命令对象不实现 Debug；日志只记录适配器 ID 和布局。错误正文供当前 UI 展示，不额外记录完整命令或控制程序输出。

当前终端恢复仍由 Agent Launcher 管理生命周期。外部终端启动控制程序的成功仅表示请求已被接受，不等于 Agent 已完成任务；任务状态由 Agent 自己报告。

## PTY 与状态协议

外部终端适配器必须为新实例提供独立 PTY；不得把 Agent stdout 改成普通 pipe 后丢弃控制序列。Usage 导出命令再次进入自己的启动入口，从而继续处理账号、配置、运行记录以及通知。

Codex 的 OSC 7501 由 Codex 直接写入自己的终端，不经过 Usage hook 或 Bark。工作、审批、提问、完成等状态属于对应 PTY。此通道与终端控制 CLI 的 stdout/stderr 不同：后者可以捕获以检查启动结果，前者必须由新终端保留。新终端支持状态协议与否需要单独验证，不能仅凭成功新建 tab 宣称支持 OSC 7501。

## 新终端接入步骤

1. 在 `src/terminals/<name>.rs` 实现 `TerminalBackend`，写明平台、活动上下文、目标布局和焦点行为。使用官方控制接口，优先取与活动服务匹配的 CLI。
2. 在 `builtin_registry()` 注册适配器，明确与其它终端嵌套时的自动检测优先级。
3. 如需配置显式选择，在 `src/config.rs` 的 `ResumeMode` 添加枚举值、ID 映射、标签和轮换顺序；保持已有配置值兼容。
4. 使用注入的 `TerminalContext` 检查自动检测、显式选择不可用、未知 ID 和平台差异，不用全局 `set_var` 驱动并行测试。
5. 用实际子进程或模拟控制 CLI 验证包含空格/引号的 cwd、命令边界、非零退出码、多实例隔离和启动失败后无重复执行。现有 Herdr / Rex 集成测试位于 `tests/support/terminal_launch.rs`。
6. 在真实终端中验证新建与恢复、焦点、Agent 退出、Ctrl+C、独立 PTY，以及 Agent 原生状态。记录本机验证范围，不把模拟测试等同于界面验证。

正常接入不应修改 Provider、账号存储、会话发现、会话恢复意图或各个 UI 页面的终端名称分支。如果新工具确实需要新布局能力，先扩充 `Capabilities`、`LaunchOptions` 与请求约定，再实现适配器；可读取 pane 拓扑的终端应返回 `adaptive` 并复用 `layout::decide`，不要在适配器里重写排布规则。

## 开发时查询 Rex 文档与社区

[Rex Community MCP](https://www.superlogical.com/rex/docs/beta/community-mcp) 提供官方文档、版本更新及 Beta Discord 历史讨论的检索。维护 Rex 适配器时可给编码 Agent 配置它，辅助排查 CLI、焦点、PTY 和状态协议问题；确认后的行为仍需落实为代码与测试。

Codex 配置命令：

```sh
codex mcp add superlogical --url https://www.superlogical.com/rex/community-mcp
```

按提示通过 Discord OAuth 授权，需要加入 Rex Beta Discord。若添加后尚未完成授权，可运行 `codex mcp login superlogical`。服务名采用官方建议的 `superlogical`，避免与 `rex` CLI 混淆。

这是开发者的可选工具，不属于 Usage 运行依赖。用户的 MCP 配置和 OAuth 凭据留在本机，不提交到仓库；终端启动和状态对接继续使用 Rex CLI、Server API 与程序原生协议。

## Ghostty

Ghostty 的 AppleScript 不暴露 pane 尺寸，也不提供稳定的“当前 Usage pane”标识。适配器先用 Usage 进程 PID 查询其 TTY，再遍历 Ghostty 的 window / tab / terminal，用 `tty of terminal` 匹配 TTY 找到 anchor；找不到 TTY 时明确报错，不向当前 focused terminal 猜测路由。随后按布局决策执行：

- 分屏：`split targetTerm direction right|down with configuration cfg`
- 标签页：`new tab in w with configuration cfg`

新 terminal 通过 `input text <command>` + `send key enter` 启动，命令保持不透明字符串。`focus = "keep"` 重新选中 Usage 的 tab 并 focus anchor；`focus = "new"` focus 新 terminal。所有 AppleScript 只把 cwd、命令、焦点等作为独立 argv 传入。假 `osascript` 测试覆盖 TTY anchor、右分屏、路径和命令边界；真实 Ghostty 的焦点行为尚未完成窗口级验证。

## Herdr 原生 App

在 Herdr App 内运行 Usage 时，适配器使用官方窗口命令，而不是 `herdr.space` / `herdr.tab` 标签约定：

1. `herdr-app ls` 找到 `HC_TERMINAL_ID` 所在 space、tab 与 pane。
2. 布局引擎决定 `pane.split --direction right|down --pane <pane> --argv […]` 或 `tab.new --space <Usage 所在 space> --name <目录名> --folder <cwd> --argv […]`。argv 为 `sh -c AGENT_WRAPPER usage-agent <cwd> <launcher command>`，cwd 和命令都是独立参数。
3. 启动成功后记录布局状态；`focus = "keep"` 先 `tab.select` Usage 所在 tab、再 `pane.focus` Usage pane，`focus = "new"` 先选中目标标签页，再聚焦新 pane。

CLI 查找顺序是 `HERDR_APP_HOME/bin/herdr-app`、PATH、`~/.local/bin/herdr-app`。查询、启动或焦点命令失败时直接报错，不重试、不切换后端。自动测试用假 `herdr-app` 覆盖多阶段调用、失败不重放和 JSON 解析；`tests/support/terminal_launch.rs` 用假 App + PTY 验证两次启动拥有不同 PTY、项目目录和恢复参数。真实 App 0.1.12 / herdr-core 0.1.17 的旧标签页实现已被官方命令实现替换；本机 App 0.1.13 / herdr-core 0.1.18 曾因 core registry writer 故障无法做真实窗口验证。

## Rex

Rex 适配器分两步：

1. `rex session inspect --json <session>` 读取窗口和 block，用 `REX_BLOCK` 定位 Usage；缺失或不在拓扑中时明确报错，不按焦点或列表顺序猜测。Rex 的窗口作为布局模型中的“标签页”，block 作为 pane。
2. `rex run --session … --cwd … --focus=false|true --keep-open --json [--block <pane> --split right|below --ratio 50] -- sh -lc <launcher command>`。`rex run` 的锚点必须显式指定，因此适配器不依赖当前焦点。

CLI 优先使用 `REX_BIN_DIR/rex`，其次 PATH。查询或启动失败不重试。自动测试覆盖 inspect 解析、block 锚点、方向、焦点、失败不重放；集成测试用假 `rex` + PTY 验证每个实例有独立 PTY、OSC 7501 原样保留、恢复参数和配置中的 layout/focus 传递。在 Rex Beta 0.1.0 (1758) 真机使用隔离测试账号和模拟 Agent 验证了分屏、标签页、keep/new 焦点和新 pane 的布局缓存；尚未验证真实 Agent、恢复会话及 OSC 状态。

## 验证边界

- Rex：官方文档确认 `rex session inspect` / `rex run --split --ratio --block` 接口；在 Rex Beta 0.1.0 (1758) 真机使用隔离测试账号和模拟 Agent 验证了分屏、标签页、keep/new 焦点和新 pane 的布局缓存；尚未验证真实 Agent、恢复会话及 OSC 状态。
- Herdr：本机 App 0.1.13 / herdr-core 0.1.18 的 `herdr-app ls/status/commands` 可用，但 core 报 `cannot save terminals: registry writer stopped`，因此无法创建真实 pane/tab 验证。
- Ghostty：AppleScript 已按本机 Ghostty 字典编写并用假 `osascript` 验证调用链；真实窗口的创建和焦点行为尚未验证。

## 重复恢复会话

通过 Usage 在外部终端恢复的会话会记录匿名启动标识与 pane 的关联。再次恢复相同来源、目标账号和命令的会话时，先读取实时拓扑；pane 仍在当前 Herdr space、Rex session 或 Ghostty window 中时直接切换标签页并聚焦，不再启动第二个 Agent。pane 已关闭则重新创建。显式切换目标账号不会复用另一个账号的 pane。普通新实例启动始终创建新实例。

关联只保存命令与项目路径的摘要以及 pane ID，不保存完整命令或凭据。更新前打开的实例、Usage 之外打开的实例和当前终端恢复没有此关联，不能仅凭会话标题猜测 pane。Rex 已退出但仍由 `--keep-open` 保留的 pane 也会被聚焦，关闭该 pane 后可重新恢复。
