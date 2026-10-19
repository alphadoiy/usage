# Provider 插件

Usage 直接运行 OpenCode Provider 插件，不依赖 magpie 或 OpenCode 主程序。Rust 管理账号、凭据、运行租约及指标，独立的 Bun/Node 子进程执行插件。内置接入包为 `@magpie-community/opencode-workbuddy-auth@0.1.24`（`workbuddy` / `workbuddy-ai`）和 `@magpie-community/opencode-trae-auth@0.2.17`（`trae-cn` / `trae-global`）。

## 安装和登录

首页 Provider 看板按 `a`，或按 `F5` 打开统一的“添加 Provider”菜单，支持 **BYOK / API Key**、**ChatGPT OAuth**、**导入 OAuth**、**从 CC Switch 导入**、**WorkBuddy**、**Trae** 和高级自定义接入。所有入口复用 `provider_setup` 的编辑、登录和导入流程，在原页面上显示二级弹窗。`F4` 统一进入 Provider 管理，在管理页按 `a` 或 `F5` 打开同一添加弹窗后选择 WorkBuddy 或 Trae。随后选择已有账号或登录新账号 → 国内版／国际版 → 登录方式 → 模型 → Provider 名称。已有账号无需重新授权；绑定后可从项目启动。

接入步骤以紧凑的二级菜单覆盖在原页面上，标题展示当前层级，Esc 返回上一级。地区附账号说明，浏览器登录标为推荐；桌面登录仅支持未加密凭据，本机凭据加密或缺失时显示原因并禁用该选项。

首次选择服务商会自动准备固定版本的底层组件；已有安装按包身份复用（支持自定义安装 ID），不会要求填写 npm 包名或版本，也不会覆盖同名的其他组件。首页按服务商及地区展示账号和额度，类型为“订阅服务”。服务商名称、地区、类型与实现包的映射集中在 `provider_catalog::presets`，后续服务商复用同一接入流程。

通用安装与自定义插件账号入口保留在“高级 · 自定义接入”中；CLI 的 `plugins` 命令继续用于开发和排查。查询可按 Esc 返回；浏览器／验证码授权复用原有认证任务，Ctrl+C 取消。API Key 输入隐藏显示，账号名称默认读取授权结果。界面和 CLI 共用安装、认证与绑定实现。

WorkBuddy 发布版可用 Node.js 22+；依赖 Bun shell、TypeScript 或 Bun API 的其他插件应显式选择 Bun。安装要求固定 SemVer 版本，禁用 npm 安装脚本，不在启动时安装或更新依赖。

```sh
usage plugins install @magpie-community/opencode-workbuddy-auth@0.1.24 --id workbuddy
usage plugins providers workbuddy
# 国内版；国际版把第二个 workbuddy 替换为 workbuddy-ai
usage plugins login workbuddy workbuddy
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins checkin <账号ID>           # 只查询状态
usage plugins checkin <账号ID> --confirm # 明确提交；执行前再次查询状态
usage plugins bind <账号ID> --name 'WorkBuddy Codex' --model <模型API-ID>
usage codex run 'WorkBuddy Codex'
```

登录默认选择插件第 0 个方法，方法及表单见 `providers`。支持浏览器自动完成、粘贴授权码与 API Key 方法；`--method <方法ID>` 选择方法，`--input key=value` 传递表单值，`--no-browser` 在终端显示授权地址而不自动打开浏览器。API Key 可用 `--key-stdin` 从标准输入读取，避免放入命令行参数。账号名称优先取插件授权结果，也可用 `--name` 指定。登录完成授权并提交后，在同一插件安装与上游地区内按稳定 UID 及企业／区域身份检测重复；再次登录同一身份会复用原账号、保存新凭据并递增 generation，保留自定义名称、推理令牌和已有绑定。昵称不作为身份；缺少稳定 UID 的插件只复用完全相同的凭据记录。旧 generation 的迟到写入会被拒绝。已有重复记录不自动合并或删除。

WorkBuddy 5.6+ 桌面凭据加密时请使用浏览器方法；宿主不解密桌面登录文件。Credits 保留原单位，不转换为美元；首页参照 Magpie 加千位分隔、最多显示两位小数，原始资源数据保留精度。插件未返回额度时展示不可用，绝不补成零。

Provider 看板的账号操作支持手动签到：支持的账号显示 `c 每日签到`，预览后按 `y` 确认提交；WorkBuddy 预览会先查询今日状态，成功后刷新额度。Esc 可取消查询或确认；提交期间禁止重复操作。首页展示支持只读状态查询的 Provider 时，会在后台自动查询并显示“今日未签到／已签到”；同账号的多个配置共用缓存，普通状态 5 分钟、已签到状态 30 分钟、失败 1 分钟后刷新。WorkBuddy 按北京时间跨日立即过期，手动 `r` 可立即更新。查询独立于额度刷新，迟到结果不能覆盖手动签到结果。没有自动领取调度，领取仍需确认。

WorkBuddy 0.1.24 未提供 `auth.checkin`，Usage 参照 Magpie 在插件宿主注册国内版签到适配器，复用插件的 loader/fetch 签名，不读取或复制 Token。国际版不展示该操作。其他插件可提供 `auth.checkin`，能力发现后复用相同的操作工厂和界面；提供只读 `auth.checkinStatus` 时才启用自动查询。仅有签到 hook 的插件不会被自动调用领取逻辑。原生 hook 优先于宿主适配器。

提交时重新查询状态，已签到或活动结束时不领取。返回 401 时探测同账号额度：额度可读则提示在官方客户端完成设备验证，额度也被拒绝则提示重新登录。网络失败或结果不明时不自动重试提交；再次操作会先查询状态。并发提交按账号串行，使用独立的操作锁，不占用推理请求的凭据刷新锁。旧版网关仍有活跃任务时保留运行，新操作提示等待它空闲退出后重试。

已知的授权失败通过本地控制协议返回 `plugin_auth` 错误码，不伪装成 HTTP 502。桌面凭据加密、桌面未登录、浏览器授权超时分别给出可执行的提示；未知原因使用通用登录失败提示。`plugin_auth_failed` 日志记录插件、上游、方法和标准错误码，不转存插件原始错误正文，避免意外带出凭据。

自动回调登录允许没有浏览器地址：WorkBuddy 的桌面登录方式返回空 URL，Usage 直接执行其本地回调，不打开浏览器。浏览器和验证码登录仍校验非空地址，验证码方式缺少地址时拒绝继续。登录日志通过 `plugin_login_started` 的 `flow`（`local`／`browser`／`code`）、插件 ID、上游 ID 和方法 ID 区分来源；地址不合法时记录 `plugin_login_rejected`，不记录授权 URL、验证码或凭据。

`bind` 创建普通托管 Codex 配置，实际订阅凭据留在插件账号库。首页展示模型、额度并复用项目与会话启动。新会话和明确绑定的恢复会话通过临时网关路由使用所选插件账号；恢复的 API 资源身份仍保持未解析，不借此推断远端账号归属。项目配置层、原生 profile 等无法确认的覆盖会阻止插件启动，避免误用其他账号。插件 Provider 必须使用网关，`USAGE_GATEWAY=off` 时明确失败。

## Trae 国内版与国际版

在添加 Provider 中选择 **Trae → 国内版 / 国际版 → 浏览器登录**。两个地区使用独立账号；授权页面回到插件在 `127.0.0.1` 临时端口监听的 `/authorize` 后自动完成，不需要粘贴令牌或填写账号名称。国际版 SG / US 路由由授权结果选择。插件本身仍标注实验性，服务商真实账号的模型权限与活动结果以接口为准。

```sh
usage plugins install @magpie-community/opencode-trae-auth@0.2.17 --id trae
usage plugins providers trae
usage plugins login trae trae-cn      # 国际版使用 trae-global
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins bind <账号ID> --name 'Trae Codex' --model <模型API-ID>
usage codex run 'Trae Codex'
```

模型列表、令牌刷新及请求签名由原版插件提供。国内版展示 Credits，国际版展示美元额度，二者不相互换算；无限额度明确标注“额度不限量”。国内版首页自动查询签到状态只访问 `/status`，不会调用领取 hook；按 `c` 再按 `y` 才调用原生 `auth.checkin`，由插件重新查询并领取。国际版虽然导出一个返回 inactive 的签到 hook，Usage 会根据地区能力声明隐藏该操作。按账号缓存与北京时间跨日失效复用现有机制。

原版 npm 包的可重复兼容性检查（不使用真实账号，不打开浏览器，全部上游请求转向本地模拟服务）：

```sh
# npm pack --ignore-scripts @magpie-community/opencode-trae-auth@0.2.17
# 将包解压到临时目录后，以原始入口运行：
cargo build
node tests/support/trae_plugin.mjs /临时目录/package/index.mjs target/debug/usage
```

此检查经过实际 Usage CLI、Rust 网关与 JS 宿主，覆盖国内版及国际版 SG/US 的浏览器回调、账号保存、动态模型目录、Credits / USD 额度、只读状态与确认签到、绑定 Agent 后的 Responses/SSE 转发。订阅凭据全部为合成数据；没有验证真实 Trae 账号或消耗真实额度。

## 能力与边界

| 接口 | 当前支持 |
| --- | --- |
| OpenCode Provider v1 | 模块导出工厂、`config`、`auth.methods`、`auth.loader`、`provider.models`、`chat.headers` |
| 宿主 client | 当前账号的 `auth.set`、`config.get`、`app.log`、`tui.showToast`；普通插件日志和 toast 不直接转入用户日志 |
| magpie Provider 扩展 | `auth.usage`、`auth.refresh`、`refreshLead`、`auth.checkin`、只读 `auth.checkinStatus` |
| 推理协议 | 插件原生 Chat / Responses；Responses 客户端到 Chat 上游的请求和响应转换 |
| 转换内容 | 文本、图片输入、指令、思考摘要、函数工具调用和结果、用量、流式/非流式 Responses 返回 |
| 显式不支持 | 未实现的 SDK client、工具/会话执行插件、OpenCode 新版插件契约、额外协议转换、服务端 `previous_response_id` 存储、内置 web search 等无法映射的工具 |

能力通过插件实际 hooks 发现，一个包可以提供多个上游。并非任意 OpenCode 插件都能运行：缺失的宿主接口、未知协议或输入能力会报错，不伪造成功。未知 SDK 协议不会自动当作 Chat 使用。使用 WorkBuddy 上游时发送完整 Responses 输入，不依赖服务端保存上下文。

协议转换在通用 `protocol.mjs` 中，不包含 WorkBuddy 分支；服务商的认证、工具参数兼容及错误解释由原版插件完成。原版插件可能修改请求文本或参数，转发语义以所安装插件版本为准。流式解析支持跨块 UTF-8、多条合并事件，单个事件最多 1 MiB，累计转换内容最多 16 MiB；断流不能产生完成事件。客户端取消会释放上游读取，慢消费者通过 Node/Bun HTTP 流背压暂停上游。

## 架构与生命周期

- `plugins::store::Store`：固定版本安装记录、账号及 opaque auth、原子写入、文件锁和凭据版本校验。插件代码不直接写账号库。
- `plugins::Runtime`：由现有网关持有，按插件配置版本加载独立宿主；每个账号的 `auth.refresh` 准备阶段串行，额度、模型及推理业务调用随后并行，慢额度查询不会阻塞同账号推理；不提供刷新扩展的插件由自身 loader 管理刷新。其他账号和插件独立执行。请求开始时固定账号，credential broker 核对插件能力令牌和凭据 generation。
- `plugins::auth`：从已发现的能力快照向 `AuthRegistry` 注册工厂，构造不执行网络或文件操作；复用 `AuthTask` 的取消与保存边界。授权结果先暂存，`begin_save` 后单独提交，取消后的迟到结果不创建账号。
- `plugins::adapters`：将用量映射为 `ResourceSnapshot`，复用 `ResourceRegistry`；模型和资源在 `CatalogService` 后台查询，首页不执行插件代码或发网络请求。
- 网关的 `RouteAuth::Plugin`：内部账号引用，不传订阅 Token 给 Agent；复用已有运行租约、HTTP/SSE 观测和性能索引。模型目录不计为生成请求。

同一 Manager 根目录复用现有网关。多个 CLI/TUI 实例共享运行中的宿主和账号刷新协调。活跃路由、插件操作或待完成的授权阻止空闲退出；手动输入授权码期间也保留登录宿主，登录完成、取消或 10 分钟到期后释放授权租约；最后一个租约/操作释放后空闲 60 秒退出。没有 LaunchAgent 或系统常驻任务。单个宿主崩溃后下次操作重新加载，不重放失败的推理请求。升级通过显式安装另一个固定版本完成，已有响应持有旧宿主直到结束，新操作使用新版本；需要回滚时安装之前的固定版本。

插件包是本机可执行代码。子进程提供崩溃隔离，未提供文件系统/网络权限沙箱；只安装信任的插件。

## 数据与排查

默认位于 `~/.local/share/usage/plugins`，遵循 `XDG_DATA_HOME`，可用 `USAGE_PLUGIN_DIR` 覆盖：

- `plugins.json`：包名、固定版本、绝对入口、运行时、启用状态、配置 revision。
- `accounts/<UUID>.json`：账号元信息、opaque auth、凭据 generation 和本地推理能力令牌；私有权限，CLI 账号列表不输出凭据。
- `descriptors/<插件ID>.json`：与配置 revision 对应的能力快照，不保存账号凭据。
- `packages/<安装ID>/`：已固定的 npm 包和依赖，安装失败不替换现有插件。
- `host/<宿主内容哈希>/`：当前 Usage 附带的宿主和协议模块。

`usage plugins list` 只读列出安装记录，不启动宿主。`enable` / `disable` 控制新操作，停用保留账号和历史；已开始的响应可以结束。损坏凭据保留原文件，拒绝覆盖；账号发现跳过损坏记录并记录 `plugin_account_read_failed`，其他账号继续展示。

日志用 `plugin_host_started`、`plugin_hook_failed` 及现有 `gateway_*` 事件关联插件、版本、账号、hook、Run、Session 和请求。`plugin_hook_failed.error_code` 仅接受宿主预定义的错误码，用于区分协议、登录表单、宿主接口与凭据代理问题；未知插件错误归为 `hook`。普通日志不包含 opaque auth、上游 Token、授权码或消息正文。插件任意日志输出不直接打印，避免插件意外泄露凭据。

## 验证

```sh
test_root=$(mktemp -d)
USAGE_CODEX_DIR="$test_root/codex" USAGE_PLUGIN_DIR="$test_root/plugins" \
  XDG_DATA_HOME="$test_root/data" USAGE_LOG=off \
  cargo test --all-targets -- --test-threads=1
node --test src/plugins/protocol.test.mjs src/plugins/actions.test.mjs src/plugins/trae_actions.test.mjs
cargo clippy --all-targets -- -D warnings
```

Rust 测试使用两个上游的合成插件、真实 Node 宿主、真实 Usage CLI 和模拟 Codex，覆盖动态认证工厂、账号隔离、同账号并发过期时仅刷新一次、慢额度查询不阻塞推理、凭据冲突、登录分阶段提交与取消、手动授权保活及到期释放、插件停用、多个配置共享账号资源、过期用量状态、新会话/恢复的网关转发及真实 Token 入库。另一个独立插件用延迟流验证原生 Responses 直通和更新后旧宿主继续完成响应。协议测试覆盖并行工具调用及结果、图片、UTF-8 分片、思考、实际 usage、非流式返回、截断和取消。测试不使用真实订阅凭据。

已验证实际 WorkBuddy 0.1.24 包加载、两个上游和能力发现；真实账号授权、模型权限及订阅推理仍需用户完成浏览器登录后验证。

账号操作通过 `provider_action_started`、`provider_action_completed` 与 `plugin_account_action_result` 关联账号、插件、上游、动作、preview/execute 阶段、标准结果及失败原因。只保留结构化状态，不记录 Token 或服务商原始错误正文。签到测试使用合成账号与模拟上游，覆盖预览无副作用、重复确认、账号隔离、不阻塞推理准备、活动状态和客户端验证；没有替用户执行真实签到。

内置账号操作适配器的包名、上游、只读查询能力和日界时区集中在 `src/plugins/action_capabilities.json`，Rust Catalog 与 JS 宿主读取同一声明；原生插件 hooks 继续动态发现。添加内置适配器时登记声明并注册 JS 实现，Catalog 不添加服务商分支。能力声明随宿主内容哈希发布，旧能力快照不会覆盖当前内置声明。

账号操作复用网关启动时的认证健康检查结果；缺少或不兼容的 `plugin_actions` 能力会在提交前明确拒绝，不重复查询状态。签到与主动重置共用 UI 确认状态转换，查询阶段允许取消，执行阶段禁止取消或重复确认；各自保留业务结果、账号隔离与刷新策略。签到 401 后的余额探测只有明确认证拒绝才显示登录过期，限流、服务故障或无效响应显示查询失败。

添加 Provider 时，保存层在 SQLite 写事务内比较规范化后的完整配置与所用凭据；仅修改名称、TOML 注释或端点末尾斜线不能重复添加。相同插件账号绑定相同模型与配置也会提示已有 Provider。不同账号密钥、模型或其他配置可独立保存；管理页的“复制”是显式创建副本，不受添加去重限制。账号登录去重在插件 Store 文件锁内执行，两个并发登录不会同时创建同一身份。
