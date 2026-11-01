# Magpie 模型连接

Usage 管理项目、Codex 启动预设、多实例进程和会话恢复。原生 OpenAI / ChatGPT OAuth 保留独立认证、账号隔离和官方直连。第三方订阅插件交给独立运行的 Magpie，Usage 不再安装 npm Provider 包、执行 Bun/Node 宿主、转换推理协议或管理第三方令牌。

## 内置入口与启动

首页和 Provider 管理始终保留唯一的 **Magpie（内置）**，无需添加，也不能复制或删除。选择它按 `e` 配置地址、模型或路由组、可选推理强度与固定账号。本机网关密钥可留空；同地址编辑时留空会保留已有密钥。添加 Provider 列表只提供 OAuth、BYOK 与导入入口。保存配置沿用现有 Profile ID，并新增配置版本；历史会话继续按其保存版本恢复。

本机默认从 `$XDG_CONFIG_HOME/magpie/settings.json`（未设置时为 `~/.config/magpie/settings.json`）读取端口，默认 3425；`USAGE_MAGPIE_URL` 可以覆盖发现地址。Usage 只读这个文件。当前远程连接接受 HTTPS，本机支持 HTTP；网关密钥保存在现有私有凭据库，通过子进程环境注入，不进入 Codex TOML 或命令参数。

```sh
usage magpie status
usage magpie models
usage magpie configure --model 'group/development'
usage magpie configure --model 'provider/model'
usage codex run 'builtin:magpie'
# HTTPS 远程网关，密钥从环境变量读取
usage magpie --address https://gateway.example --key-env MAGPIE_GATEWAY_KEY models
```

目录分三种入口：`group/<id>` 是用户在 Magpie 配置的路由方案，可以按意图、上下文等规则选模型；`auto/<model>` 是 Magpie 聚合的同名模型供应来源；`provider/model` 是指定来源的模型。Usage 将 ID 原样发送，路由规则、账号调度、后备模型和推理强度决策均在 Magpie 内执行。没有配置路由组时不会显示虚构的“自动”方案。

指定来源不代表固定账号，也不代表关闭 Magpie 的后备规则。`configure --account <账号标识>` 将 `X-Magpie-Account` 固定到实例配置；账号标识由 Magpie 提供，不是 Usage OAuth 账号 ID。未指定时由 Magpie 调度。

启动配置使用显式的 `model_provider = "magpie"` 标识模型连接，通过现有 Manager 保存版本。已有多个旧 Magpie 预设时，界面收敛到一个内置入口，原配置及历史保留；后续配置修改沿用同一个 Profile。启动前验证 `/api/hello` 和实时目录，生成该实例的 Codex `model_catalog_json`，并拒绝已移除的模型。Agent 直接连接 Magpie，即使 `USAGE_GATEWAY=off` 也能运行。原生全局 Codex 配置不被改写；原生 Agent 子进程环境继续清除继承的认证覆盖。更改 Usage 配置从下一次启动生效，更改 Magpie 路由规则按 Magpie 的轮次语义生效。

## 看板与观测

内置卡片的“用量与余额”直接展示 Magpie 共享额度、余额及今日用量，不再额外生成资源卡。额度接口和用量汇总接口各查询一次；首次使用即使尚未配置启动模型，也能读取本机 Magpie 的目录与资源。服务离线时保留内置入口并展示连接错误。

首页只展示账号额度进度、剩余额度、余额及今日请求数和 Token 总量，失败次数仅在非零时显示。Credits 最多保留两位小数；余额保留供应商返回的单位。读取时间、统计范围、估算费用和 Token 分类说明不占用首页。查询失败和未知值不补成零，两个接口独立刷新，其中一个失败不隐藏另一个的结果。

`/v1/magpie/quotas` 和 `/v1/magpie/usage?period=today` 的完整返回仍可通过 CLI 查询。用量是 Magpie 全部记录，可能包含其他客户端和本地会话，不代表当前预设独占的消耗；汇总中的 USD 费用是已定价部分的估算，不是账号余额扣费。

首页选择 Magpie 卡片按 `v` 打开今日请求记录：支持翻页、刷新、查看模型、账号、Token、首字、请求耗时和可定价请求的估算费用。选中带 Session 的请求按 Enter，读取该会话的**最新**路由、实际模型、推理强度、规则和尝试列表。最新路由不是所选历史请求的路由快照。

```sh
usage magpie quotas
usage magpie summary --period today
usage magpie requests --period today --limit 50
usage magpie requests --session <session-id>
usage magpie route <session-id>
```

CLI Session 查询先请求 Magpie 的搜索分页，再精确筛选 Session；返回的是该页匹配行，不声称是会话总量。看板的请求范围是整个连接，可能包含其他 Agent 的请求；不推断某个 Usage Run 的消耗。Usage 仍独立记录本地 Run、PID、退出结果和经过核验的会话关联，Magpie 观测不作为进程存活证明。

受限制的网关密钥可能无权查询所有账号额度或汇总，接口返回的权限错误会直接展示。接口缺失提示更新 Magpie，连接失败提示确认服务运行；没有将失败数据伪装为正常空值。

## 旧插件预设迁移

先在 Magpie 安装相应插件并授权，读取其目录，再显式映射目标模型。旧插件凭据不自动复制，账号也不按昵称猜测匹配。

```sh
usage codex list --json
usage magpie models
usage magpie bind --replace '<旧预设名称或 ID>' --name '迁移后的名称' --model 'provider/model'
```

`--replace` 通过 Manager 乐观版本更新保留 Profile ID、历史配置版本、Run 和会话。明确拒绝把原生 OAuth 账号转成 Magpie。旧 `usage-plugin.invalid` 绑定在看板显示迁移提示，新启动会拒绝执行；旧凭据文件不被删除。已经运行的旧版进程继续拥有其原有生命周期，不由迁移主动终止。

## 架构与验证

`src/magpie` 是外部服务适配层，负责内置入口、连接身份、协议 DTO、Codex 目录投影、资源读取与配置。`provider_catalog::sources` 只读装配唯一内置卡片；配置时才写入 Manager。UI 和 CLI 共享添加、复制、删除的限制。Magpie 不注册成一个新的 Agent，原生 OAuth 与普通 BYOK 的启动接口继续复用。普通 BYOK 仍可使用现有本地观测，Magpie 路径完全绕过它。

`tests/support/magpie.py` 用本地合成服务、真实 Usage CLI 和模拟 Codex，验证动态目录、并行直接启动、固定账号、恢复、观测接口、模型移除、原地迁移及终端交互；`src/magpie/tests.rs` 验证目录能力、单位转换、历史保留及 OAuth 拒绝迁移。测试不使用真实订阅推理。

```sh
cargo test --test magpie
cargo test --lib magpie::
# 可选：使用真实 Codex 检查配置、模型目录和线程创建（无真实模型请求）
python3 tests/support/magpie.py target/debug/usage /path/to/codex
```
