# 第一阶段优化记录（2026-10-15）

本轮修复初次消融复现的四个扩展契约，保留工厂与注册表的拆分。原始数据仍保存在 [优化前评估](provider-stage1-ablation.md) 和对应 JSON 中，未改写为通过结果。

## 行为调整

| 路径 | 优化后行为 |
| --- | --- |
| 刷新与卡片 | 通过 `UsageProvider::cache_key()` 对应的资源身份分发结果，同类型账号不再覆盖第一张卡。工具 ID 继续用于样式和共享服务状态。未知目标事件不会消耗 pending 或触发提醒 |
| 低额度提醒 | 状态键包含资源身份；无邮箱、相同邮箱的不同连接独立触发。原时间顺序、冷却及恢复逻辑保持原有约定 |
| 主动重置 | 确认流程保存资源身份，消费时按该身份定位，防止第二个账号被错误定位为同工具的第一个账号 |
| 当前终端恢复 | 扫描得到的 Session 保留构造它的 Agent，恢复经其 Launcher；Codex 托管 Home 的账号与版本解析由内置启动适配器处理 |
| 复制与分屏恢复 | Launcher 显式声明命令导出能力；自定义启动器不支持时返回错误，避免直接执行元数据命令。Ghostty 统一复用已转义的导出命令及 `command_in_ghostty`，删除第二套分屏 AppleScript |
| Provider 会话查询 | `Provider::sessions()` 使用定义中绑定的 Home/托管账号版本，不再接收任意目录。返回会话的实例 ID 为 Provider ID，并保留该 Provider 的 Agent |
| 多资源查询 | 按 Tokio task ID 找回原资源位置；panic、超时逐项转换为绑定原账号的临时失败快照。正常结果保留，输出维持配置顺序 |
| 查询截止时间 | typed 资源路径由 Hub 统一限制，移除订阅适配器中的重复超时包装。旧用量入口继续使用原有 `query_with_timeout` |
| 故障日志 | `resource_query_failed` 包含服务类型、资源身份哈希、任务 ID 和固定故障原因；不记录凭据或 panic 内容 |

旧 `SessionRegistry::register(SessionSource)` 及手工创建的 Session 可不携带 Agent，以兼容已有 loader/元数据调用；需要自定义启动逻辑时应通过 `SessionRegistry::from_agents` 注册，或使用 Provider 生成的目录。导出命令受 Launcher 独立能力约束，不宣称任意进程内自定义启动器都能转为可复制命令。

## 验证

- 四个原失败契约迁入正常测试：`hub::regression_tests`、`ui::app::account_regression_tests`。
- UI 回归经过真实的异步刷新分发，验证两张卡分别收到各自账号结果，且两个无邮箱账号各产生一条提醒。
- 提醒回归同时覆盖无邮箱和相同邮箱；重置回归确认第二个账号消耗次数，第一个保持零次。
- 资源回归覆盖 panic、永久 pending 的适配器，以及健康结果、失败绑定与配置顺序的保留。
- 会话回归验证已注册 Launcher 被调用一次，未提供命令导出能力时明确失败。原有 Home 环境变量、shell 转义及托管账号运行回归继续通过。
- 完整回归：199 项单元测试 + 24 项集成测试，共 **223 项通过**；配置隔离于 `/private/tmp/usage-optimize-verify-20261015`。需要监听回环端口的既有模拟服务在沙箱外运行，无真实推送。
- `cargo fmt --all`、`cargo clippy --offline --all-targets -- -D warnings`、`git diff --check` 通过。
- 消融脚本改为执行正式回归模块；Python 语法及全部 12 个变体修改锚点检查通过。本轮未重跑优化前的整个消融矩阵，旧 JSON 仍只描述旧快照。

## 仍待后续迁移

当前用量首页尚未接实际 BYOK 余额服务。New API/官方 API 连接配置、资源查询消费者和首页迁移仍需实现。

通用 Run 模型、非阻塞运行句柄、CLI 覆盖后的实际配置与 Run 到会话的绑定仍未完成。启动 helper 每次构造默认 Hub、CLI/TUI 已有 Manager 与适配器再次打开 Manager、旧用量兼容往返包装也仍是可优化项；本轮没有用旁路调用替换这些接口。后续应在应用入口注入运行服务和注册表，避免增加第二套账号启动实现。
