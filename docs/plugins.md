# Provider 插件

Usage 直接运行 OpenCode Provider 插件，不依赖 magpie 或 OpenCode 主程序。Rust 管理账号、凭据、运行租约及指标，独立的 Bun/Node 子进程执行插件。首个接入包为 `@magpie-community/opencode-workbuddy-auth@0.1.24`，同时提供 `workbuddy` 与 `workbuddy-ai`。

## 安装和登录

WorkBuddy 发布版可用 Node.js 22+；依赖 Bun shell、TypeScript 或 Bun API 的其他插件应显式选择 Bun。安装要求固定 SemVer 版本，禁用 npm 安装脚本，不在启动时安装或更新依赖。

```sh
usage plugins install @magpie-community/opencode-workbuddy-auth@0.1.24 --id workbuddy
usage plugins providers workbuddy
# 国内版；国际版把第二个 workbuddy 替换为 workbuddy-ai
usage plugins login workbuddy workbuddy
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins bind <账号ID> --name 'WorkBuddy Codex' --model <模型API-ID>
usage codex run 'WorkBuddy Codex'
```

登录默认选择插件第 0 个方法，方法及表单见 `providers`。支持浏览器自动完成、粘贴授权码与 API Key 方法；`--method <方法ID>` 选择方法，`--input key=value` 传递表单值，`--no-browser` 在终端显示授权地址而不自动打开浏览器。API Key 可用 `--key-stdin` 从标准输入读取，避免放入命令行参数。账号名称优先取插件授权结果，也可用 `--name` 指定。每次登录创建独立账号，现有账号凭据不会被静默覆盖。

WorkBuddy 5.6+ 桌面凭据加密时请使用浏览器方法；宿主不解密桌面登录文件。Credits 保留原单位，不转换为美元。插件未返回额度时展示不可用，绝不补成零。

`bind` 创建普通托管 Codex 配置，实际订阅凭据留在插件账号库。首页展示模型、额度并复用项目与会话启动。新会话和明确绑定的恢复会话通过临时网关路由使用所选插件账号；恢复的 API 资源身份仍保持未解析，不借此推断远端账号归属。项目配置层、原生 profile 等无法确认的覆盖会阻止插件启动，避免误用其他账号。插件 Provider 必须使用网关，`USAGE_GATEWAY=off` 时明确失败。

## 能力与边界

| 接口 | 当前支持 |
| --- | --- |
| OpenCode Provider v1 | 模块导出工厂、`config`、`auth.methods`、`auth.loader`、`provider.models`、`chat.headers` |
| 宿主 client | 当前账号的 `auth.set`、`config.get`、`app.log`、`tui.showToast`；普通插件日志和 toast 不直接转入用户日志 |
| magpie Provider 扩展 | `auth.usage`、`auth.refresh`、`refreshLead` |
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
node --test src/plugins/protocol.test.mjs
cargo clippy --all-targets -- -D warnings
```

Rust 测试使用两个上游的合成插件、真实 Node 宿主、真实 Usage CLI 和模拟 Codex，覆盖动态认证工厂、账号隔离、同账号并发过期时仅刷新一次、慢额度查询不阻塞推理、凭据冲突、登录分阶段提交与取消、手动授权保活及到期释放、插件停用、多个配置共享账号资源、过期用量状态、新会话/恢复的网关转发及真实 Token 入库。另一个独立插件用延迟流验证原生 Responses 直通和更新后旧宿主继续完成响应。协议测试覆盖并行工具调用及结果、图片、UTF-8 分片、思考、实际 usage、非流式返回、截断和取消。测试不使用真实订阅凭据。

已验证实际 WorkBuddy 0.1.24 包加载、两个上游和能力发现；真实账号授权、模型权限及订阅推理仍需用户完成浏览器登录后验证。
