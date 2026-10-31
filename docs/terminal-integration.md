# 终端适配规范

终端是 Agent 启动命令的传输目标，与 Provider、Agent 类型、账号凭据和会话来源分离。实现集中在 `src/terminals/`。首页、会话恢复和 Provider 管理使用相同入口；`sessions` 中的旧函数只做兼容委托。

## 当前行为

| 目标 | 检测上下文 | 执行方式 | 布局与焦点 |
| --- | --- | --- | --- |
| Rex | 非空 `REX_SESSION`；读取 `REX_BIN_DIR`、`REX_SERVER` | 匹配服务器的 `rex run --session … --cwd … --focus=false --keep-open -- sh -lc COMMAND` | 每次新建 tab，保留 Usage 焦点 |
| Ghostty | macOS，且有 `GHOSTTY_RESOURCES_DIR` 或 `TERM_PROGRAM=ghostty` | AppleScript 将 cwd 和命令作为独立 argv 传入 | 右侧分屏并聚焦 |
| 当前终端 | 显式选择，或自动检测未命中 | 调用方退出 TUI，委托原 Agent Launcher，退出后返回 TUI | 复用当前终端 |

自动检测按注册顺序执行，Rex 优先于 Ghostty，避免嵌套环境中残留的 Ghostty 标记抢占 Rex。仅安装某个终端不代表当前运行在其中。显式选择不可用终端时报错，不改为其它终端，也不在失败后再次启动 Agent。

## 结构与设计模式

```text
Agent Launcher / ResumePlan
    │ 导出包含标识符的恢复或启动命令
    ▼
TerminalRequest { cwd, shell_command }
    ▼
TerminalRegistry + Selection + TerminalContext
    │ 选择策略、校验 cwd、绑定环境快照
    ▼
TerminalBackend（Adapter / Strategy）
    │ 只构造 std::process::Command
    ▼
TerminalLaunch（Command）
    │ 统一执行与失败处理
    ▼
终端自己的 PTY → Agent → 原生终端协议
```

- **Adapter / Strategy**：`TerminalBackend` 封装工具特有的检测和启动命令。Rex CLI 与 Ghostty AppleScript 在独立文件中，不进入业务层。
- **Registry**：`TerminalRegistry` 管理有序策略，拒绝重复、空或保留 ID。内置注册表初始化后只读；测试可注入独立注册表，无需修改进程全局环境。
- **Command**：`prepare` 返回 `TerminalLaunch`，构造阶段不启动终端；`execute` 才运行控制程序。执行失败直接返回，避免重复启动。
- **依赖倒置**：UI 只使用目标描述与 `is_current()` 判断是否需要交还当前终端，不根据 Rex、Ghostty 等具体名称分支。配置字符串到适配器 ID 的映射集中于 `ResumeMode::backend_id`。

这里使用轻量对象注册表，不引入动态插件加载、额外继承层次或重复的工厂体系。

## 接口约定

| 接口 | 职责 |
| --- | --- |
| `target()` | 返回稳定 ID、界面描述和布局类型 `Tab / RightSplit` |
| `environment_keys()` | 声明检测及控制 CLI 需要的环境变量；注册表只采集这些变量 |
| `available(context)` | 只读检查平台和活动上下文；不启动进程、不探测网络、不创建窗口 |
| `command(context, request)` | 根据已校验请求构造控制命令；不执行命令，不再次读取全局环境 |
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
5. 用实际子进程或模拟控制 CLI 验证包含空格/引号的 cwd、命令边界、非零退出码、多实例隔离和启动失败后无重复执行。现有 Rex 集成测试位于 `tests/support/terminal_launch.rs`。
6. 在真实终端中验证新建与恢复、焦点、Agent 退出、Ctrl+C、独立 PTY，以及 Agent 原生状态。记录本机验证范围，不把模拟测试等同于界面验证。

正常接入不应修改 Provider、账号存储、会话发现、会话恢复意图或各个 UI 页面的终端名称分支。如果新工具确实需要新布局能力，先扩充 `Placement` 与请求约定，再实现适配器。
