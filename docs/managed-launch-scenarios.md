# Usage 托管启动场景

从 Usage 选择 Provider 启动，即选择该实例的账号通道和启动模型。优先级为：本次明确的模型/effort 参数 > 实例运行配置（含 Codex 在该实例保存的偏好）> 允许继承的全局偏好。账号通道只能通过 Usage 的 Provider 配置修改。

这些约束用于托管启动及明确选择 Provider 的恢复；本地原生恢复继续遵循 Codex 自身的配置规则。

| 场景 | 输入 | 预期行为 |
| --- | --- | --- |
| 用户目录启动插件 | cwd 为 `~`，`~/.codex/config.toml` 指向官方账号；Usage 选择 Trae | 启动 Trae，使用独立 Home、插件凭据和网关，不因文件存在而拒绝 |
| 项目配置冲突 | 受信任项目设置其他模型、effort 和 Provider | Usage 已设置的模型、effort 和通道通过启动参数固定；项目其他设置由 Codex 处理 |
| 全局偏好继承 | 全局配置含官方认证、MCP 和界面偏好 | 排除全局账号通道；保留允许继承的 MCP、界面等偏好；原文件不变 |
| 多实例并行 | 两个账号或版本在同一目录启动 | Home、凭据、上游和网关令牌各自独立 |
| BYOK 普通参数覆盖 | `-c model=...`、effort、普通 feature 开关 | 正常启动，记录本次明确选择；普通 `-c` 不使网关失效 |
| 通道参数覆盖 | `-c model_provider=...`、provider 表、认证或共享状态路径 | 启动前明确拒绝，错误不包含凭据值；防止后追加参数绕过网关 |
| 其他原生 profile/本地 Provider | `--profile`、`--oss` 等 | 托管启动明确拒绝改变配置来源；通过 Usage 选择或编辑 Provider |
| 参数边界 | 图片/输出文件名看起来像参数，或 `--` 后的提示词 | 按原生参数边界处理，不误判为通道覆盖 |
| 会话恢复 | 选定 Trae 恢复其他来源会话 | 固定本次选定实例的通道和启动模型；历史来源不变，实际模型证据未确认前不伪造归属 |
| 不支持的网关传输 | 明确启用当前网关不支持的 WebSocket feature | 插件启动失败并显示实际原因；普通 BYOK 可按既有规则跳过可选观测 |
| 官方订阅实例 | 从 Usage 选择 OAuth 账号 | 保持独立认证、官方直连，通用项目配置存在不阻止启动 |

实现约束：不复制 Codex 的信任和配置分层算法；不删除全局文件、不更改工作目录、不关闭项目配置。固定 Usage 拥有的字段；原生仍可能读取项目配置，语法错误及管理员强制要求继续由原生报告。启动侧配置身份与运行中的实际请求证据分别记录。

## 验证方式

- `cargo test`：参数边界、身份、原生恢复、并发账号及插件网关回归。插件集成用例在同一个模拟用户目录中配置官方通道，再执行插件启动和恢复，检查实际子进程收到的有效模型/effort、继承偏好及网关请求。
- `python3 tests/support/managed_launch_config.py target/debug/usage /path/to/codex`：通过真实 Codex 的 `config/read` 检查用户目录、项目目录、并行账号和显式模型覆盖；临时 Home、合成凭据，不发起模型推理。
- `cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check`。

另一个既有脚本 `native_codex_context.py` 在当前安装的 Codex 0.166.0 上，跨 Provider 的 `thread/resume` 返回 `modelProviderNotConfigured`；本机已安装的旧版 Usage 同样复现。该兼容问题独立于本次配置优先级修复，不能将该脚本标记为通过。
