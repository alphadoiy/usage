# Provider 插件

Usage 直接运行 OpenCode Provider 插件，不依赖 magpie 或 OpenCode 主程序。Rust 管理账号、凭据、运行租约及指标，独立的 Bun/Node 子进程执行插件。

| 服务 | 固定包版本 | 上游 |
| --- | --- | --- |
| WorkBuddy | `@magpie-community/opencode-workbuddy-auth@0.1.24` | `workbuddy` / `workbuddy-ai` |
| Trae | `@magpie-community/opencode-trae-auth@0.2.17` | `trae-cn` / `trae-global` |
| ZCode | `@magpie-community/opencode-zcode-auth@0.1.40` | `zcode` |
| Qoder | `@magpie-community/opencode-qoder-auth@0.2.52` | `qoder` / `qoder-cn` |
| CodeArts 码道 | `@magpie-community/opencode-codearts-auth@0.3.0` | `codearts` |
| 商汤小浣熊 | `@magpie-community/opencode-raccoon-auth@0.1.5` | `raccoon` |
| MiniMax Code | `@magpie-community/opencode-minimax-auth@0.1.7` | `minimax-code` / `minimax-code-global` |


## 安装和登录

首页 Provider 看板按 `a`，或按 `F5` 打开统一的“添加 Provider”菜单，支持 **BYOK / API Key**、**ChatGPT OAuth**、**导入 OAuth**、**从 CC Switch 导入**、**WorkBuddy**、**Trae**、**ZCode**、**Qoder**、**CodeArts**、**商汤小浣熊**、**MiniMax Code** 和高级自定义接入。所有入口复用 `provider_setup` 的编辑、登录和导入流程，在原页面上显示二级弹窗。`F4` 统一进入 Provider 管理，在管理页按 `a` 或 `F5` 打开同一添加弹窗后选择对应服务商。随后选择已有账号或登录新账号 → 国内版／国际版 → 登录方式 → 模型 → Provider 名称。已有账号无需重新授权；绑定后可从项目启动。

接入步骤以紧凑的二级菜单覆盖在原页面上，标题展示当前层级，Esc 返回上一级。地区附账号说明，浏览器登录标为推荐；桌面登录仅支持未加密凭据，本机凭据加密或缺失时显示原因并禁用该选项。

首次选择服务商会自动准备固定版本的底层组件；已有安装按包身份复用（支持自定义安装 ID），低于内置要求时在同一安装 ID 下升级，保留账号与绑定，较新版本不降级，不会要求填写 npm 包名或版本，也不会覆盖同名的其他组件。首页按服务商及地区展示账号和额度，类型为“订阅服务”。服务商名称、地区、类型与实现包的映射集中在 `provider_catalog::presets`，后续服务商复用同一接入流程。

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

从 Usage 启动插件绑定的 Codex 时，宿主会为该会话注入当前账号专属的 `model_catalog_url`。Codex 的 `/model` 因此显示插件提供的模型，而不是内置的 GPT 目录；新会话会重新查询。目录来自插件的模型 hook，插件可能在上游查询失败时使用自己的静态回退列表，因此单个模型的推理权限仍以实际请求结果为准。Codex 的全局 `model_catalog_json` 设置会优先于远程目录，使用插件账号时不要设置它。

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

## ZCode / GLM Coding Plan

添加 Provider 中选择 **ZCode → GLM Coding Plan**，支持 Z.ai 浏览器登录、智谱浏览器登录、团队订阅、读取 ZCode 应用登录，以及 GLM Coding Plan API Key。浏览器方式自动轮询完成，不需要粘贴授权码；团队方式先选择网站。API Key 表单同时显示网站和隐藏密钥输入，网站选择与凭据一起保存。

```sh
usage plugins install @magpie-community/opencode-zcode-auth@0.1.40 --id zcode
usage plugins providers zcode
usage plugins login zcode zcode                    # Z.ai；智谱使用 --method 1
usage plugins login zcode zcode --method 2 --input site=zai # 团队订阅
usage plugins login zcode zcode --method 3          # 读取 ZCode 应用账号
usage plugins login zcode zcode --method 4 --input site=bigmodel --key-stdin
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins bind <账号ID> --name 'ZCode Codex' --model <模型API-ID>
usage codex run 'ZCode Codex'
```

参照 Magpie 固定使用原版 0.1.40 包，模型发现、订阅路由、团队凭据和赠送计划均由插件处理。ZCode 应用登录由插件读取本机数据，仅跟随同一账号；宿主不写入 ZCode 的凭据文件。仅使用 API Key 无法访问 ZCode 赠送计划；`-Trial` 模型是否可用取决于插件查询到的登录和赠送额度。ZCode 0.1.40 提供每日活动包领取，首页按 `c` 并确认后执行；没有活动返回 inactive，遇验证码需要在官方应用完成，纯 API Key 账号没有领取资格。不会自动领取。

ZCode 使用 Anthropic Messages 协议。通用宿主把 Codex Responses 转成 Messages，支持系统指令、文本、图片、命名空间函数／自由文本工具及结果、思考摘要、缓存令牌用量，以及流式／非流式响应。断流或上游错误不能产生成功完成事件。结构化输出当前明确拒绝；内置服务端搜索沿用 Chat 插件的限制。Responses 历史中的思考摘要没有 Anthropic 签名，因此不作为签名思考块回传。

原版包兼容性检查使用合成账号及本地模拟上游，覆盖 Z.ai／智谱浏览器和 API Key、团队与桌面登录、模型目录、额度、Codex 启动及 Messages 转发；不访问真实服务商或消耗订阅额度：

```sh
# npm pack --ignore-scripts @magpie-community/opencode-zcode-auth@0.1.40
# 解压后运行原始包入口：
cargo build
node tests/support/zcode_plugin.mjs /临时目录/package/index.mjs target/debug/usage
```

## Qoder 国内版与国际版

添加 Provider 中选择 **Qoder → 国内版 / 国际版 → 浏览器登录**，在对应地区网站授权后自动轮询完成。国内版使用 qoder.cn（阿里云或手机号）账号，国际版使用 qoder.com 账号；两个地区账号独立。两者均支持个人访问令牌（PAT），国内版还支持企业 VPC 登录，填写企业实例名称或地址后在企业网站授权。PAT 输入隐藏显示。

```sh
usage plugins install @magpie-community/opencode-qoder-auth@0.2.52 --id qoder
usage plugins providers qoder
usage plugins login qoder qoder-cn       # 国际版使用 qoder
# 国内企业 VPC：--method 2 --input vpc=acme
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins bind <账号ID> --name 'Qoder Codex' --model <模型API-ID>
usage codex run 'Qoder Codex'
```

固定使用 magpie 所引用的原版 0.2.52 包。设备授权、PAT 交换、单次使用的刷新令牌、企业路由、模型发现、COSY 签名及 Qoder 请求编码由原版插件处理；Usage 复用账号库与 Responses → Chat 转换。额度保留 Credits 单位，显示订阅、加购和共享额度。浏览器登录与 PAT 登录属于同一 UID 时复用账号，保留原名称及绑定。

原生 `auth.checkin` 仅在用户确认后调用。插件没有只读签到状态 hook，首页不会自动查询或领取；签到预览只提示确认，不表示已查询今日状态。领取时插件重新读取活动；设备身份要求由插件处理，可能需要本机安装并登录对应地区的官方客户端。Usage 不另行读取、生成或改写官方设备凭据。包中额外导出的 QwenWork 服务保留在高级自定义接入中，不属于 Qoder 的地区选项。

原版包兼容性检查使用合成账号和本地模拟上游，不访问真实 Qoder 账号或消耗额度：

```sh
# npm pack --ignore-scripts @magpie-community/opencode-qoder-auth@0.2.52
# 解压后运行原始包入口：
cargo build
node tests/support/qoder_plugin.mjs /临时目录/package/index.mjs target/debug/usage
```

检查覆盖全部原版导出的加载、国内／国际浏览器和 PAT 登录、企业 VPC、凭据刷新落库、动态模型目录、Credits、多种登录的账号复用、无领取副作用的签到预览，以及 Codex 流式／非流式 Responses、工具调用与缓存用量。真实账号授权、模型权限和签到活动仍以服务商实际结果为准。

## CodeArts、商汤小浣熊与 MiniMax Code

添加 Provider 中选择对应服务商，选择浏览器登录并在官方授权页完成操作，然后选择模型保存。CodeArts 使用华为云 OAuth，本机接收回调；小浣熊支持浏览器自动返回、读取本机桌面同一账号，以及手动回调／Token 备用入口；MiniMax 分国内版和国际版，使用对应网站的设备码授权。读取小浣熊桌面登录不会消耗客户端刷新令牌，客户端切换账号不会替换 Usage 席位的身份。

```sh
usage plugins install @magpie-community/opencode-codearts-auth@0.3.0 --id codearts
usage plugins login codearts codearts
usage plugins install @magpie-community/opencode-raccoon-auth@0.1.5 --id raccoon
usage plugins login raccoon raccoon
usage plugins install @magpie-community/opencode-minimax-auth@0.1.7 --id minimax
usage plugins login minimax minimax-code # 国际版使用 minimax-code-global
usage plugins accounts
usage plugins models <账号ID>
usage plugins usage <账号ID>
usage plugins checkin <账号ID>           # 预览，不领取
usage plugins checkin <账号ID> --confirm # 明确确认领取
usage plugins bind <账号ID> --name '我的 Provider' --model <模型API-ID>
```

三个服务均通过原版 `auth.checkin` 领取，不在后台自动调用。当前原包未导出只读签到状态 hook，因此首页不自动查今日状态，预览只提示确认；执行时由原包读取活动并领取。小浣熊签到可能同时领取一次性的桌面登录奖励；CodeArts 每日签到包含领取和确认两步，确认失败的记录由原包保存供下次处理。MiniMax 读取一周签到面板后领取当天积分，国内与国际账号独立。

CodeArts 的免费模型还有一套单独的每日权益：原版插件在获取免费模型或遇到 benefit not found 时会领取它。这不属于 Usage 的手动签到操作；使用该插件查询目录或推理时保留此原版行为。CodeArts 仅声明文本输入。插件额外导出的 OfficeAce 保留在高级自定义接入中，没有加入 CodeArts 地区菜单。

Credits 和积分保留原单位，不换算成美元。仅返回余额时展示实际余额，不补总额度或使用百分比。CodeArts 的签到记录保存在 Usage 插件目录的 `state/<安装ID>/codearts-checkins.json`，不写 magpie 配置目录。

CodeArts 0.3.0 与小浣熊 0.1.5 尚未发布到 npm，Usage 从 `magpie-community/plugins` 的 dist 分支下载对应固定版本 tgz，并核对代码中固定的 SHA-512，随后通过 npm `--ignore-scripts` 安装；包名、版本再次校验通过后才替换安装记录。下载或校验失败不会替换原插件，也不会静默换版本。

升级已有 ZCode 可以重新打开添加入口选择 ZCode，或显式执行安装命令；账号 ID、凭据与已有 Agent 绑定保留：

```sh
usage plugins install @magpie-community/opencode-zcode-auth@0.1.40 --id zcode
usage plugins providers zcode # 刷新能力快照，让首页识别新版签到入口
```

原包兼容性检查只使用合成账号、模拟浏览器回调和本地上游，覆盖登录、刷新落库、动态目录、额度、签到确认／已签到／失败，以及 Codex 的 Responses/SSE、工具调用和缓存用量：

```sh
cargo build
node tests/support/subscription_plugins.mjs codearts /临时目录/codearts/package/index.mjs target/debug/usage
node tests/support/subscription_plugins.mjs raccoon /临时目录/raccoon/package/index.mjs target/debug/usage
node tests/support/subscription_plugins.mjs minimax /临时目录/minimax/package/index.mjs target/debug/usage
```

真实账号授权、活动资格与模型权限仍需实际登录后验证；模拟测试没有领取真实账号积分或消耗真实额度。

## 能力与边界

| 接口 | 当前支持 |
| --- | --- |
| OpenCode Provider v1 | 模块导出工厂、`config`、`auth.methods`、`auth.loader`、`provider.models`、`chat.headers` |
| 宿主 client | 当前账号的 `auth.set`、`config.get`、`app.log`、`tui.showToast`；普通插件日志和 toast 不直接转入用户日志 |
| magpie Provider 扩展 | `auth.usage`、`auth.refresh`、`refreshLead`、`auth.checkin`、只读 `auth.checkinStatus` |
| 推理协议 | 插件原生 Chat / Responses；Responses 客户端到 Chat / Anthropic Messages 上游的请求和响应转换 |
| 转换内容 | 文本、图片输入、指令、思考摘要、函数与自由文本工具调用和结果、命名空间工具、客户端工具搜索、用量、流式/非流式 Responses 返回 |
| 显式不支持 | 未实现的 SDK client、工具/会话执行插件、OpenCode 新版插件契约、额外协议转换、服务端 `previous_response_id` 存储、内置 web search 等无法映射的工具 |

能力通过插件实际 hooks 发现，一个包可以提供多个上游。并非任意 OpenCode 插件都能运行：缺失的宿主接口、未知协议或输入能力会报错，不伪造成功。未知 SDK 协议不会自动当作 Chat 使用。WorkBuddy 客户端发送完整 Responses 输入，宿主将其转换为 Chat 请求交给插件，不依赖服务端保存上下文。

协议转换在通用 `protocol.mjs` 中，不包含 WorkBuddy 分支；服务商的认证、工具参数兼容及错误解释由原版插件完成。原版插件可能修改请求文本或参数，转发语义以所安装插件版本为准。流式解析支持跨块 UTF-8、多条合并事件，单个事件最多 1 MiB，累计转换内容最多 16 MiB；断流不能产生完成事件。客户端取消会释放上游读取，慢消费者通过 Node/Bun HTTP 流背压暂停上游。

参照 Magpie 的 `textcalls.go` 和 `phase.go`，转换层会将文本中完整的 Hermes JSON、DeepSeek DSML、GLM `arg_key` / `arg_value` 调用恢复为请求已声明的工具；未知工具、无法解析的参数和残缺调用保持原文，不猜测工具或命令。调用前的文字在完成事件中标记 `commentary`，正常完成且没有后续工具的最后文字标记 `final_answer`；截断或失败不会补最终回答阶段。这些标记不能判断模型的文字是否真正完成用户任务，也不自动发送“继续”。

插件请求的短重试参照 Magpie `passing()`：429 临时限流最多再试三次，默认等待 1、2、4 秒；408、500、502、503、504、529 和响应头返回前的连接错误最多再试两次。尊重 `Retry-After` 秒数或日期，单次超过 8 秒即返回原错误；额度、余额和上下文错误不自动重试。已经开始交付的流不会自动重放。该策略针对本次绑定的账号，不自动改用其他账号、模型或订阅，也不包含 Magpie 路由组的 Patience 和候选切换。

Chat 上游的工具名按 `namespace__name` 展平，自由文本工具转换为带 `input` 字符串参数的函数；响应再恢复原始命名空间和工具类型。客户端执行的 `tool_search` 也按函数调用发送，搜索结果里的工具会加入后续请求。Codex 在普通请求中附带的服务端 `web_search` 不能由 Chat 插件执行；Usage 当前没有代搜后端，因此不将它声明给插件，历史中的搜索事件也不伪装成客户端工具调用。普通请求继续运行，但此路径不提供实时搜索；明确强制该工具的请求会报告搜索不可用。工具调用名可能分多个流块返回，宿主收到完整调用后再发送 Responses 工具事件。

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

`plugin_inference_diagnostic` 按 `run_id`、`session_id` 和 `request_id` 关联以下阶段：`upstream_response`（插件归一化前的原始 HTTP 状态、上游请求 ID、数字错误码）、`response`（每次插件响应的 HTTP 状态、耗时、上游请求 ID、数字错误码及错误分类）、`retry`（次数和等待毫秒数）、`upstream_stream`（原始 SSE 事件数、是否见到结束标记、EOF / 中断 / 取消）、`converted`（转换后的结束原因、工具数、文本恢复工具数、文字和思考字符数）、`delivery`（交付完成 / 中断 / 取消）。上游错误正文只在内存中用于重试分类，不写入日志；诊断接口仅接受固定枚举、数值和受限格式的关联 ID，报告失败不影响推理。

尤其需要对照原始流和转换结果：Trae 原版插件可能在上游没有明确结束标记而自然 EOF 时补 `stop`，此时同一请求可以同时出现 `upstream_stream.outcome=eof_without_terminal` 和 `converted.outcome=completed`。Usage 保留原版插件行为并记录差异，不能把 `gateway_request_finished.complete=true` 单独当作服务商原始流完整结束的证据。

## 验证

```sh
test_root=$(mktemp -d)
USAGE_CODEX_DIR="$test_root/codex" USAGE_PLUGIN_DIR="$test_root/plugins" \
  XDG_DATA_HOME="$test_root/data" USAGE_LOG=off \
  cargo test --all-targets -- --test-threads=1
node --test src/plugins/protocol.test.mjs src/plugins/inference.test.mjs src/plugins/actions.test.mjs src/plugins/trae_actions.test.mjs
cargo clippy --all-targets -- -D warnings
```

Rust 测试使用两个上游的合成插件、真实 Node 宿主、真实 Usage CLI 和模拟 Codex，覆盖动态认证工厂、账号隔离、同账号并发过期时仅刷新一次、慢额度查询不阻塞推理、凭据冲突、登录分阶段提交与取消、手动授权保活及到期释放、插件停用、多个配置共享账号资源、过期用量状态、新会话/恢复的网关转发及真实 Token 入库。另一个独立插件用延迟流验证原生 Responses 直通和更新后旧宿主继续完成响应。协议测试覆盖并行工具调用及结果、图片、UTF-8 分片、思考、实际 usage、非流式返回、截断和取消。测试不使用真实订阅凭据。

已验证实际 WorkBuddy 0.1.24 包加载、两个上游和能力发现；真实账号授权、模型权限及订阅推理仍需用户完成浏览器登录后验证。

账号操作通过 `provider_action_started`、`provider_action_completed` 与 `plugin_account_action_result` 关联账号、插件、上游、动作、preview/execute 阶段、标准结果及失败原因。只保留结构化状态，不记录 Token 或服务商原始错误正文。签到测试使用合成账号与模拟上游，覆盖预览无副作用、重复确认、账号隔离、不阻塞推理准备、活动状态和客户端验证；没有替用户执行真实签到。

内置账号操作适配器的包名、上游、只读查询能力和日界时区集中在 `src/plugins/action_capabilities.json`，Rust Catalog 与 JS 宿主读取同一声明；原生插件 hooks 继续动态发现。添加内置适配器时登记声明并注册 JS 实现，Catalog 不添加服务商分支。能力声明随宿主内容哈希发布，旧能力快照不会覆盖当前内置声明。

账号操作复用网关启动时的认证健康检查结果；缺少或不兼容的 `plugin_actions` 能力会在提交前明确拒绝，不重复查询状态。签到与主动重置共用 UI 确认状态转换，查询阶段允许取消，执行阶段禁止取消或重复确认；各自保留业务结果、账号隔离与刷新策略。签到 401 后的余额探测只有明确认证拒绝才显示登录过期，限流、服务故障或无效响应显示查询失败。

添加 Provider 时，保存层在 SQLite 写事务内比较规范化后的完整配置与所用凭据；仅修改名称、TOML 注释或端点末尾斜线不能重复添加。相同插件账号绑定相同模型与配置也会提示已有 Provider。不同账号密钥、模型或其他配置可独立保存；管理页的“复制”是显式创建副本，不受添加去重限制。账号登录去重在插件 Store 文件锁内执行，两个并发登录不会同时创建同一身份。
