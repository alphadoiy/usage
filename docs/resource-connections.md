# 资源连接：余额、额度与费用

资源连接与 Agent 类型分开配置。当前已接入 `new_api_token`、`new_api_account` 和 `openai_costs`，通过 `usage resources` 使用；原 TUI 首页尚未迁移到这些快照。

## 配置和查询

在 `~/.config/usage/config.toml` 增加以下配置。`credential_env` 保存环境变量名称，查询时读取其值；不在 TOML、命令行参数、输出或日志中保存密钥。

```toml
[[resources]]
id = "relay-token"
label = "工作中转站令牌"
service = "new_api_token"
base_url = "https://relay.example"
credential_env = "RELAY_USAGE_KEY"
options = {}
# 可选：显式关联 usage codex list --json 中的账号 UUID
# profile_ids = ["123e4567-e89b-12d3-a456-426614174001"]
# 密钥在同一环境变量中轮换后，可更新此值以改变资源版本身份
# revision = "2"
```

在当前 shell 中设置 `RELAY_USAGE_KEY` 后运行：

```bash
usage resources --list
usage resources relay-token
usage resources --json
usage resources --profile 工作账号 --json
usage --config /path/to/config.toml resources --json
```

不指定资源 ID 时查询所有连接；`--profile` 接受 Codex 账号名称或 UUID，只查询显式关联的连接。普通 `--list` 校验配置和服务工厂，但不读取资源凭据或请求网络。`--run <UUID>` 查询启动时匹配的连接，需额外配置 `inference`，见 [运行资源关联](run-resource-binding.md)；此模式即使只列出连接也会核对凭据，但不会请求网络。

返回码：全部成功或未配置连接为 0，至少一个资源查询失败为 1，配置、选择或工厂错误为 2。不同资源并发查询，输出保持配置顺序；某个资源失败不丢弃其他资源的结果。

`base_url` 是该查询协议的基础 URL，支持站点的部署子路径。New API 使用站点根路径，例如 `https://relay.example/panel`，不要填写推理端点 `/v1`。OpenAI 官方费用使用 `https://api.openai.com/v1`。不尝试根据推理端点自动识别服务或轮询其他接口。URL 需为 HTTPS，本机回环 HTTP 可用于本地服务和测试；不接受 URL 中的凭据、查询参数和片段。

## 服务及权限范围

| service | 查询 | 凭据和范围 |
| --- | --- | --- |
| `new_api_token` | `GET api/usage/token` | 推理 API token，返回该令牌自身的额度；不限量不代表账号余额无限 |
| `new_api_account` | `GET api/user/self` | 站点账号的管理访问凭据，另需 `options.user_id`；发送 `New-Api-User` 并核对返回的账号 ID |
| `openai_costs` | `GET organization/costs` | OpenAI Admin API Key，查询组织费用；可显式筛选项目或 API Key ID |

New API 默认输出原始 `new_api_quota` 单位，不把它当作模型 token 数或美元余额。若已确认该站点的换算设置，可显式提供比例和币种：

```toml
[[resources]]
id = "relay-wallet"
label = "工作中转站账号"
service = "new_api_account"
base_url = "https://relay.example"
credential_env = "RELAY_ACCOUNT_ACCESS_TOKEN"
options = { user_id = 42, quota_per_unit = "500000", currency = "USD" }
```

这里的 `500000` 只是显式配置示例，不是程序假设的全局换算比例。`quota_per_unit` 是每一货币单位对应的原始额度，必须为正十进制字符串，并同时填写三位大写币种代码。换算后仍保留原始额度。令牌不限量时不生成有限的金额余额；过期令牌返回认证失败，不能用其额度表示可用状态。

OpenAI 官方费用配置：

```toml
[[resources]]
id = "openai-project-costs"
label = "OpenAI 工作项目"
service = "openai_costs"
base_url = "https://api.openai.com/v1"
credential_env = "OPENAI_USAGE_ADMIN_KEY"
options = { days = 30, project_id = "proj-your-project" }
```

`days` 默认 30，可设 1–180；`project_id` 与 `api_key_id` 最多填写一个，均省略时范围是组织。这里的 API Key ID 是平台的非秘密标识，不是密钥值。查询带固定 UTC 时间范围，跟随分页，按币种分别累计；第二页失败、分页循环或超出上限时，不把部分合计当作完整费用输出。空账单返回成功且无费用项目，不猜测币种或充值余额。

金额与原始额度使用十进制文本；换算和费用累加使用 `rust_decimal`，不经二进制浮点数。超出可表示的数值范围按格式错误处理。

## 身份、关联与诊断

每个连接有唯一 `id`。资源绑定版本由服务、规范化端点、凭据环境变量名称、显式 revision 和服务选项计算摘要；修改查询范围或换算设置不会继续使用旧版本身份。当前新查询没有持久缓存，也不回退到其他账号或过期快照。

`profile_ids` 仅表示用户配置的关联，不证明该资源凭据与某次运行最终采用的凭据相同。新增的 `inference` 声明用于启动时核对端点与凭据并保存版本关联；`resources --run` 仅接受启动时已匹配且未变更的连接。查询器不会读取 Codex 私有密钥文件，也不会将组织 Admin API Key 注入 Agent。会话绑定及 Agent 回传的实际配置仍待贯通。

HTTP 状态、查询状态、连接 ID、资源身份哈希和独立查询操作 ID 写入结构化日志。并发任务继承入口追踪上下文；HTTP 事件与查询结果可用操作 ID 和资源标识关联。日志不写 Authorization、凭据值、响应原文和查询 URL；服务错误以固定原因表达，防止服务端错误文本回显密钥。

## 扩展方式与验证

资源服务通过 `ResourceRegistry` 注册。`ApiProtocol` 提供服务自身的配置校验、权限范围和响应解析；共享 `ApiClient` 负责凭据读取与 HTTP，`ResourceService` 和 Provider 共用并发查询/超时收集。新增协议增加适配器及注册项，无需修改全局协议枚举或 CLI 分派。

测试使用注入的 FakeHttp 和本机 HTTP 服务，覆盖工厂延迟读取凭据、多连接隔离、原始大整数、币种换算、不限量、过期、账号 ID 核对、HTTP 错误、分页、精确费用、CLI 输出、账号关联及日志关联。尚未使用真实站点或官方管理密钥进行联调，不宣称所有 New API 分支都兼容。

协议依据：[New API 令牌文档](https://doc.newapi.pro/en/api/token-usage/)、[New API 账号实现](https://github.com/QuantumNous/new-api/blob/main/controller/user.go)、[OpenAI Costs](https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/usage/methods/costs)、[OpenAI Administration](https://developers.openai.com/api/reference/administration/overview)。接口核对日期：2026-10-15。参考 magpie 的账号/令牌两类查询边界，本实现保留显式协议和换算设置。
