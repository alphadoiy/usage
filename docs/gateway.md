# 本地请求观测网关

Usage 启动能够确认配置来源的托管 Codex BYOK 新会话时，会按需启动本地网关。BYOK 使用临时注入的地址和本地路由令牌，网关再使用该次启动选定的真实端点与凭据转发请求。账号配置、保存版本和原生用户配置不被改写，首页切换 Provider 不会改变已有路由。

官方订阅（ChatGPT OAuth）始终直连 OpenAI，不经过网关，也不注入 `openai_base_url` 或代理绕过设置；日志记录 `gateway_skipped`（`official_direct`）。其请求性能来自会话日志：Codex 在响应结束时写入的 `token_usage_record` 提供逐请求结束时间，回答项的 `started_at_ms` 提供首字，详见[Provider 性能](performance.md)。下文第二阶段的 OAuth 路由已移除，相关记录保留作历史说明。

```text
Usage 启动 → 解析原始 Provider / 账号 / 版本 → 注册本次启动路由
Agent → 127.0.0.1 网关 → 已绑定的推理端点
                    → 有界指标队列 → 性能索引 → 首页 / v 详情
```

## 使用

正常通过 Usage 启动受支持的 BYOK 新会话即可，无需手动启动服务。重新启动 Agent 才能接入，已经运行的会话不会被追溯接管。

```sh
usage gateway status
usage logs --tail 100
```

`gateway status` 只查询，不创建后台服务；运行时返回服务 PID、监听端口、路由数量和活跃请求数量，不输出令牌或上游密钥。

若要为一次启动关闭网关，可执行：

```sh
USAGE_GATEWAY=off usage codex run <账号>
```

默认开启仅适用于支持运行租约继承的 Unix 启动器。当前接入范围：

- 托管 Codex BYOK 新会话，且原始推理端点、模型提供方、凭据引用可由现有启动解析器确认。
- HTTP/SSE 的 Responses、Chat Completions 流量；实际 CLI 仍需支持其保存的协议。`models` 和 `responses/compact` 原样转发，不计入本期生成性能统计。
- OAuth 新会话直连官方服务，不接入网关。
- 恢复会话、本地未托管实例、项目 `.codex/config.toml`、原生 profile、显式 `-c` 覆盖、未知启动选项、BYOK 显式 WebSocket 或强制系统代理配置等继续使用原有调用路径。默认 WebSocket 升级请求返回 426，引导 Codex 回退 HTTP/SSE；单次启动可选择 WS 观测。跳过时记录 `gateway_skipped` 及原因。保持原生认证，不修改保存的配置。

独立资源连接仍通过显式绑定的日志实例查询历史性能，不会因展示余额自动代理流量。

## 服务与路由生命周期

同一个 Manager 根目录下的多个启动共享一个后台服务，支持并发绑定不同账号、Provider 和端点。启动锁避免多个 Usage 进程同时创建服务；服务锁与带认证、协议版本检查的健康检查识别可复用实例。

网关是可降级的观测能力。冷启动准备预算 1.5 秒：同时启动的实例在预算内等待启动锁，健康探测超时 150 ms，路由注册使用剩余预算（最后一次控制调用、线程及文件系统调度可带来少量额外耗时）。目录异常、服务不可用、注册失败、版本不兼容或租约准备失败时，记录 `gateway_fallback`，保留原始端点、凭据和启动参数，继续直连启动 Agent。只有路由和租约准备成功后才改写子进程环境及临时地址。

性能库不在转发的必经路径上：网关启动时恢复旧运行范围失败只记录 `gateway_recovery_skipped`；登记运行失败时照常注册路由并记录 `gateway_run_unregistered`，该运行改用会话日志统计（与写入失败时的回退相同），插件账号因此不会因性能库不可用而无法推理。

准备路由本身不排除 CLI 日志；只有同一事务成功写入实际网关请求记录后，才启用该次启动的来源优先范围。降级直连和采集初始化失败不会让真实 CLI 记录消失。

服务独立于 TUI 进程。启动器持有文件租约，并将租约继承给 Agent；退出首页、释放进程句柄或启动器意外退出，不会释放仍由 Agent 持有的租约。服务定期清理已释放且没有进行中请求的路由；所有路由空闲 60 秒后退出。进行中的请求不受空闲回收影响。

运行文件位于 `<Manager 根目录>/gateway`，目录权限 700，文件权限 600。`state.json` 只保存本地端口与控制令牌；BYOK 上游密钥只存在服务内存。上游固定为已注册的地址，禁止 HTTP 重定向，调用方不能把它当作任意目标代理。服务仅监听 `127.0.0.1`，路由也需要独立令牌。BYOK 使用本地 Bearer；OAuth 使用不可猜测的路由路径能力令牌，并核对原生 `chatgpt-account-id`，保留请求本身的 Bearer，拒绝工作区不匹配、API Key 误用和跨目标令牌转发。Agent 的 `NO_PROXY` / `no_proxy` 临时增加回环地址，并保留原有规则。

网关异常退出可能中断当前正在传输的请求，交给 Agent 原生错误处理与重试机制；网关不自动重放，避免重复执行或计费。下一次 Usage 启动会检查失效状态并重新启动；恢复失败则直连启动，不等待服务恢复。已启动 Agent 不会被偷偷重启或改写认证。新服务按自己的 Manager 根目录收束上次未完成的采集范围，不影响其他 Manager 的服务。

## 指标与数据来源

网关用单调时钟记录请求开始、首内容、首文本、首可见推理和完成事件。Responses / Chat Completions 的最终 usage 提供输入、输出、缓存读取及推理 Token；SSE 分块数和文本字节数不作为 Token 数。

- 平均首字：请求开始至首个有效内容事件，不把响应创建、角色声明、保活当成首字。
- TPS：没有推理 Token 时，按输出 Token / 首内容至完成的窗口计算；有推理时优先按回答 Token / 首文本至完成计算。只有确实观察到可见推理及持续流动时，才允许使用对应推理窗口。
- 延迟的 EOF、客户端收到完成事件后关闭连接，不会额外扩大生成窗口或抹掉已完成样本。
- 参考 Magpie 的最小窗口 100 ms、上限 10,000 tok/s、10%—90% 内容到达分布和推理/回答速度比规则，排除短窗口和集中突发。内容到达分布以字节加权，仅用于判断缓冲现象，不估算 Token。
- 缺少 usage、未完成、没有可用流式计时或证据不足时，不产生 TPS 样本。非流式 JSON 可以记录 usage，但没有首字/TPS。中断、失败请求保留状态与结果元数据，不能伪造成功性能样本。

这些是客户端链路观察指标，包含上游转发、网络和缓冲影响，不代表模型服务器内部的解码速度。首页保留原有 5 分钟摘要缓存，`v` 性能弹窗按现有刷新周期读取新记录；网关写入不依赖弹窗是否打开。

指标写入现有 `performance.sqlite3` 的独立网关表，保存请求、启动、会话、请求模型与上游报告模型、响应 ID、状态、Token、时序和缺少速度的原因。正文、思考原文、工具内容和认证信息不落库。未知会话身份计入未归属，可由已验证的原生 SessionStart 补充启动关联。

默认仅使用 HTTP/SSE 采集。通过鉴权的 WebSocket 握手在本地返回 426，不联系上游、不登记生成请求，也不切换统计来源；Codex 的后续 HTTP 请求正常采集。旧版本曾透明转发的 WS 启动保留 CLI 来源选择兼容，避免历史数据重复计数。

网关记录优先于同一会话、已登记启动生命周期范围内的 CLI 日志。这个范围是来源选择，不是按相近时间合并请求；不同请求、不同尝试始终保留。该会话在范围外直接恢复时，CLI 日志仍参与统计。网关记录通过本次请求的稳定 ID 保证重复写入不重复计数。

## 性能与排查

请求正文上限 32 MiB；按协议重建 HTTP 传输头，正文原样转发；不做模型映射、协议转换、自动重试或故障切换。默认 WebSocket 升级返回 426，Codex 回退 HTTP/SSE；显式选择 WS 时建立原始字节通道并被动观察消息。连接超时 10 秒，等待上游响应头最多 120 秒；超过时返回 504。响应流连续无数据 120 秒后结束并记录 `stream_timeout`，未完成回复不生成成功性能样本。上游 429/5xx 状态、错误正文和 Retry-After 等端到端响应头直接转发，由 Agent 决定如何处理，网关不增加重试。大于 1 MiB 的单个 SSE 事件停止指标解析，仍继续转发；非流式 JSON 只在 1 MiB 内解析指标。压缩正文不会为采样而解压改写，可能无法提取指标。

每条流最多保存有限事件缓冲和 128 个流动采样点，不累计完整回复。指标通过 256 条有界队列后台写入 SQLite；采集或写入失败不阻塞回复，丢弃或失败会记录日志，并将该次启动整体回退到会话日志统计，避免部分网关样本覆盖完整记录。来源回退独立于样本队列，由后台批量持久化；存储失败保留待写状态并定期重试。退出时按 250 ms 软预算排空正常队列，剩余样本选择 CLI 来源，再做一次来源写入；仍失败的标记保存到网关目录的 `pending-sources`，随后释放服务锁，不因指标库长期不可写而无限等待。恢复批次每份最多 256 个来源、64 KiB，只含作用域和运行 ID；新服务定期幂等重放，损坏文件保留为 `.invalid`。软预算不保证文件系统操作的硬超时；恢复文件本身也无法写入时记录错误并退出。来源状态最多保留 256 项（包括尚未写入的已结束启动），达到上限时新启动直连，不影响主流程。服务最多保留 256 条活动路由。

主要日志事件：

- `gateway_injected` / `gateway_skipped` / `gateway_fallback`：本次启动是否接入、不支持的原因，或准备失败后保留原始配置直连的原因。
- `gateway_route_registered` / `gateway_route_expired`：路由绑定与回收。
- `gateway_request_started` / `gateway_request_finished`：请求 ID、启动 ID、Provider、会话、请求与报告模型、状态、完成结果、Token、首字、首文本、流动窗口及缺少 TPS 的原因。
- `gateway_record_failed` / `gateway_record_dropped`：存储失败或队列容量问题。
- `gateway_cli_source_selected` / `gateway_cli_source_write_failed`：该次启动回退到会话日志统计；来源写入失败时保留状态并重试。
- `gateway_cli_sources_deferred` / `gateway_cli_sources_recovery_failed` / `gateway_cli_sources_recovery_save_failed`：待恢复来源已保存、重放暂时失败，或恢复文件也无法保存。

自动化验证使用本地模拟上游与真实 Usage 子进程，覆盖并发账号隔离、同一服务复用、启动器被终止后的租约、请求和响应透传、鉴权、原始凭据不入日志、流式拆包、缺失 usage、集中突发、跨来源重复统计和恢复范围隔离。自动化测试不会向真实厂商发送推理请求；经用户授权的手动联调另行记录。


## 第一阶段降级验收

自动化用例使用模拟上游，不请求真实厂商。新增覆盖启动锁被占用、运行目录异常、性能索引初始化失败时仍直连启动，验证原始端点和凭据未被本地令牌污染；模拟上游响应头超时、流内停顿和 429，验证及时结束、状态和 Retry-After 透传且不额外重试；杀死网关进程后由下一次新启动重新建立服务，验证不会重放旧启动的请求。

60 秒空闲策略通过可推进的时间参数验证边界和活跃路由重置；端到端另行验证运行租约和路由回收。没有为这项回归真实等待 60 秒，也没有把真实 Codex/中转站联调作为开发启动的前置条件。实际厂商兼容性仍需在正常使用中观察日志。

## 第二阶段：OAuth 原生认证与传输（已移除）

> 2026-10-24 起官方订阅一律直连，以下 OAuth 路由及其验证仅作历史记录；WebSocket 426 引导与逐请求采集仍适用于 BYOK。

OAuth 路由固定为官方 Codex 后端，或经过控制接口显式注册的本机回环测试服务；普通外部服务不能接收 OAuth 透传令牌。Agent 的登录文件、模型提供方、原生 profile 和刷新行为保持不变，保存的配置不写入网关 URL。本地路径能力令牌不输出到文件日志。

采用公开的 [`openai_base_url` 配置](https://learn.chatgpt.com/docs/config-file/config-reference) 保留内置提供方；[官方认证说明](https://learn.chatgpt.com/docs/auth)中的原生登录机制仍由 CLI 执行。具体后端路由及无 Content-Type 的 SSE 行为参考 Magpie 的 `internal/gateway/codex_backend.go`；模拟服务覆盖边界条件，真实服务验证记录见下文。采集值仍是客户端链路指标。

新增验证覆盖 OAuth 跨工作区拒绝、连续请求使用刷新后的原生令牌、缺少 Content-Type 的 SSE、真实子进程临时参数与原始登录刷新保留、网关准备失败后原生直连，以及 WebSocket 426 后 HTTP/SSE 正常采集。恢复会话、更多 Agent 和复杂配置层仍属后续覆盖；WS 逐请求采集见下一节。

### WebSocket 逐请求性能采集

默认仍为 SSE。需要保留 CLI 原生 WebSocket 时，可对一次启动设置：

```sh
USAGE_GATEWAY_TRANSPORT=ws usage codex run <账号>
```

`websocket` 也是可用值。选择保存在本次注册路由中，同一服务内的其他启动保持自己的选择；不强制 CLI 使用 WS、不修改配置、不自动重放请求。未选择 WS 的握手仍返回 426。

WS 握手、掩码、分帧、控制帧及协议扩展原样转发；被动解析文本消息，支持协商的 permessage-deflate（双向独立上下文、context takeover 与 no-context-takeover）、TCP 拆包、WS 续帧和穿插的 Ping/Pong。压缩只在观察副本中解码，不修改线上字节。每个 response.create 单独开始计时；按 stream_id 分流并按 response ID 验证归属，同一流按 FIFO 处理。握手耗时、请求间空闲和连接寿命不计入该次首字延迟。response.completed 立即保存指标，无需等连接关闭。generate=false 预热不计为生成请求。

首字、Token usage、回答/推理窗口、缓冲过滤和 TPS 均复用 SSE 的 Observer，消息数或帧数不换算为 Token。未完成、取消、失败、429 和断线不生成成功速度样本。记录附带 transport=websocket 和独立 connection_id，关联请求、响应和启动；正文、请求 JSON 和凭据不落库。每个请求优先读取 client_metadata.thread_id，握手/HTTP 的 thread-id 或旧版 session_id、x-session-id 作为回退；不使用 prompt_cache_key 或当前 CLI 的 session-id（后者可代表缓存亲和性）推断线程。多路请求可携带不同的 thread_id。

压缩载荷、续帧累计和解压后的单条观察消息各有 1 MiB 上限，按帧边界逐步读取，不因 TCP 合包而拒绝合法消息；每条连接最多观察 64 个待完成请求，并保留有限的已结束响应 ID。未支持的扩展、无效压缩、二进制消息、超限、steering 自动续接及无法可靠关联的事件只停用观测，继续原样转发，并记录 gateway_websocket_observation_fallback 及原因；该启动整体使用 CLI 统计，避免将部分 WS/HTTP 指标与 CLI 相加。旧版本的透传来源标记继续兼容。成功观测的 WS 请求与 SSE 一样进入性能摘要。

自动化验证覆盖同连接多轮独立计时、并发流和同流排队、完成后不等断连、预热、重复终态 ID、错误/未完成/取消/断线、推理与 SSE 计算一致、掩码和长帧拆包、续帧与控制帧、超限/扩展回退、压缩上下文与解压膨胀限制、未知字节继续转发，以及真实本地套接字的鉴权、握手和原始字节一致性。上游握手 429 原样返回状态、正文和 Retry-After，不登记生成请求、不增加重试。

首次手动复测曾因模型列表与 workspace discovery 超时、chatgpt.com TLS 连接失败而中止。网络恢复后继续实测，发现并补齐了真实 CLI 的 permessage-deflate 压缩观察，以及线程归属字段的兼容性。

### 真实 WS 指标联调（2026-10-18）

使用已保存的真实 OAuth、Codex CLI 0.163.0 和 gpt-6.1-sol，经正常 `usage codex run` 启动路径、设置 `USAGE_GATEWAY_TRANSPORT=ws`，在临时目录完成真实请求。证据已核对到性能库中的同一线程：

- 实际协商 permessage-deflate，先预热再生成；最终仅记录 1 条生成请求，预热不重复计数。
- 输出 243 Token、输入 17253 Token，均与 CLI 的 turn.completed usage 一致。
- 网关记录首字 3167 ms、生成窗口 4973 ms，TPS 约 48.86，status=200、complete=true，且没有 CLI 来源回退标记。
- 入库 session 与 CLI thread.started 的 thread_id 完全一致，修复前空归属的问题已在真实请求中消除。
- 原有配置和登录文件均未改变；本次相关应用日志未发现 access token、refresh token 或 ID token。

首次重试真实 WS 时发现压缩观察缺口；补齐后又通过入库核对发现线程归属缺口，随后增加相应用例并重新完成真实请求，以上数值取自最终线程归属正确的请求。当前 Codex 的 session-id 可以表示缓存亲和性，不能用作线程 ID，参见 [Codex 0.163 的实现](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/core/src/client.rs#L661)。

随后移除 USAGE_GATEWAY_TRANSPORT 设置，用同一版本、同一 OAuth 复测默认路径：日志确认一次 426 后回退 HTTP/SSE，CLI 正常退出。数据库中的线程与 CLI 一致，输入 17255 Token、输出 243 Token，首字 3334 ms、生成窗口 3480 ms、TPS 约 69.83，status=200、complete=true。配置及登录文件未改变，相关应用日志未发现 OAuth 凭据。两次不同模式启动共享服务，但各自遵守路由中的传输选择。

这些是单次链路样本，不作为 SSE 与 WS 的性能对比；真实长期多轮、休眠重连和令牌刷新未在此次验证覆盖。

### 真实 OAuth 联调（2026-10-18，切换默认 SSE 前）

经用户授权，使用当前已保存的 OAuth 登录、Codex CLI 0.163.0 和 gpt-6.1-sol 完成两次最小请求：

- 正常 `usage codex run ... -- exec --json --skip-git-repo-check` 在临时目录执行，CLI 原生选择 WebSocket，完整返回 243 个输出 Token，退出码 0。日志确认路由绑定到 chatgpt.com；数据库写入原生传输来源标记，未将握手伪造为 TPS 样本。原有 TOML 配置与登录文件均未改变。
- 使用同一 OAuth 的当前访问令牌，在独立临时网关和性能库中直接验证 HTTP/SSE，不改 CLI 的传输选择。上游返回 200 且缺少 Content-Type；实际收到 319 个文本增量和 response.completed。数据库记录 44 个输入 Token、323 个输出 Token、首字 3525 ms、生成窗口 3709 ms，TPS 为约 87.09，完成状态和 usage 均可核对。
- 检查本次相关应用日志，未发现 access token、refresh token 或 ID token。WebSocket 关闭日志的 clean_transport_close 仅表示字节通道是否正常关闭，不代表模型回复是否完成。
- 独立测试网关在最后路由回收约 60.06 秒后自行退出，并删除 state.json，真实等待验证了空闲退出策略。

首次启动暴露了 CLI 自动向独立 OAuth profile 写入 tui.screen_reader_detection_done 后被误判为配置篡改的问题；OAuth 现复用已有的运行时字段校验，允许经过类型检查的原生状态，仍拒绝模型、认证和路由改动，且不覆盖运行时文件。新增回归覆盖这一行为。

本次凭据未发生刷新，真实刷新流程与恢复会话尚未在此次联调覆盖；刷新透传仍由模拟上游和子进程回归验证。上述时延和 TPS 是单次链路样本，不作为性能基准。


## 插件推理后端

[Provider 插件](plugins.md) 复用此网关。`RouteAuth::Plugin` 按账号引用宿主，真实订阅凭据由 credential broker 提供给插件，Agent 仅持有本次路由令牌。绑定配置使用不可直连的 `usage-plugin.invalid` 标识，启动时替换为临时路由；网关准备失败、被禁用或无法确认配置时明确阻止插件启动，不尝试直连。

插件响应进入相同的有界 SSE 观测和性能索引。插件操作计入空闲判断；网关退出时释放宿主，宿主崩溃后下一次操作可重建，失败请求不重放。明确绑定的插件恢复可以固定本次传输，恢复的资源身份仍保留未解析状态。普通 BYOK/OAuth 的原有跳过与直连策略不变。
