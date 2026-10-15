# 运行配置与资源关联

启动路径和资源查询路径通过启动时保存的 `RunContext` 连接。此功能目前针对 Usage 托管的 Codex BYOK 启动；首页仍未迁移。

## 配置和使用

为资源连接显式声明它对应的推理身份：

```toml
[[resources]]
id = "relay-token"
service = "new_api_token"
base_url = "https://relay.example"
credential_env = "RELAY_TOKEN"
profile_ids = ["123e4567-e89b-12d3-a456-426614174001"]
options = {}
inference = { base_url = "https://relay.example/v1", credential_env = "RELAY_TOKEN" }
```

`inference.base_url` 是明确的推理端点，与资源查询的站点根地址不同；程序不会拼接或去除 `/v1` 来猜测它。`inference.credential_env` 只保存变量名称，用于启动时在内存中核对运行实际采用的密钥。令牌范围的资源还要求查询 token 与推理 token 值相同。

账号余额和官方费用使用独立管理凭据时，`credential_env` 填查询凭据的变量名称，`inference.credential_env` 填推理密钥的变量名称。这里的配置表达用户确认的账号／项目关系；本地端点与密钥核对不证明远端组织成员关系，也不证明某次运行独占整个账号余额或项目费用。

```bash
usage codex run 工作账号 --cwd /path/to/project
usage codex runs 工作账号 --json
usage resources --run <运行 UUID> --list --json
usage resources --run <运行 UUID> --json
usage resources relay-token --run <运行 UUID>
```

`--profile` 只按当前配置中的账号关联筛选，`--run` 使用该次运行启动时的匹配记录，两者不能同时使用。`--run` 可查询仍在运行或已经结束的记录；资源结果是当前查询时的余额／费用，不能据此计算该次运行的消耗。

`--run --list` 不请求网络，但会读取环境变量以核对身份。普通 `--list` 不读取密钥。未知运行、旧运行、没有匹配连接或关联失效返回配置错误（退出码 2），不回退到账号关联或其他连接。

## 启动时保存什么

`usage codex runs --json` 的每条记录新增 `context`：

- `identity`：启动侧解析状态、Agent、认证方式、端点、模型、模型提供方、凭据来源类别及未解析原因。
- `resources`：启动时该账号关联的连接、资源服务、配置版本摘要及匹配状态。
- `resources_error`：启动时无法加载 Usage 资源配置的固定原因；此错误不阻止原有启动流程。

匹配状态包括 `matched`、`endpoint_mismatch`、`credential_mismatch`、`missing_credential`、`missing_inference_binding`、`invalid_resource_configuration` 和 `unresolved_identity`。仅有 `profile_ids` 时会记录关联，但不会记录成已匹配。

数据库以新增 `run_contexts` 表保存快照，与原 Run 在同一事务提交。旧数据库自动添加该表，旧运行的 `context` 为 null，不根据当前配置补写历史身份。原 `revision` 继续表示选中的保存版本，参数覆盖后的身份单独保存在 `context.identity`，不改写账号版本。

不保存原始启动参数、完整覆盖配置或密钥值。凭据校验摘要以每次运行的独立 ID 和用途区分，仅保存在权限为 0600 的管理数据库中，不进入 CLI JSON、日志或资源缓存键。查询时重新核对摘要；连接版本、账号关联或凭据改变后拒绝查询。通过核对的查询凭据固定在本次查询上下文中，HTTP 不会再次从环境读取另一份凭据。

## 配置解析边界

支持保存的 BYOK 配置、`--model` / `-m` 和 `--config` / `-c` 的路径覆盖、连续覆盖及带引号的 TOML 路径。表值赋值替换目标表，不将其误当作深度合并。显式模型参数优先于配置中的模型。选中的环境凭据固定注入子进程；已被托管启动清除的认证环境变量不能被当作可用凭据。

`resolved` 表示启动器完成了受支持来源的解析，尚不是 Agent 回传的运行时确认。以下情况明确返回 `unresolved`，仍允许原有启动，但不绑定资源：

- 原生 profile、项目 `.codex/config.toml`、工作目录覆盖或 worktree。
- 会话恢复、OAuth 运行以及未支持的子命令或选项。
- 自定义认证／路由字段、隐式模型默认层、无法解析的配置或缺少运行凭据。

子命令检查区分命令层级，包括 `exec` 及其别名 `e` 下的 `resume`、`fork` 和 `review`；这些形式同样不绑定资源。普通提示词（如 `exec "resume the migration"`）和 `--` 后的参数不因包含命令名称而被误判。

不会推断交互中切换模型／提供方后的身份，也不会模拟完整的 Codex 配置加载器。后续通过 Agent 事件取得实际配置后才能扩展这些场景。显式 Usage 配置路径已经沿 CLI、账号 TUI 与分屏启动传递；会话恢复目前统一标记未解析。

结构化日志以运行操作 ID 关联 `run_identity_captured` 与逐连接的 `run_resource_link`，记录状态、固定原因和配置摘要，不记录端点、覆盖参数、密钥或凭据校验摘要。

## 验证与依据

自动测试使用临时数据库、合成密钥、模拟子进程及本机 HTTP 服务，覆盖同账号并行使用不同端点和密钥、只覆盖端点、令牌身份不符、配置与凭据变更、显式配置路径、旧数据库及日志／JSON／数据库不含明文密钥。尚未使用真实 Codex 服务端事件确认配置，也未进行真实站点联调。

配置规则依据 OpenAI Docs：[配置优先级](https://learn.chatgpt.com/docs/config-file/config-basic)、[配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)、[CLI 参数](https://learn.chatgpt.com/docs/developer-commands?surface=cli)。核对日期：2026-10-16。

本阶段消融发现与正式回归迁移见 [修复记录](provider-stage3-optimization.md)。
