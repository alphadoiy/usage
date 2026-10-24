# 官方订阅模型指纹测试

参考 [LM Fingerpoint Detector 检测文档](https://lm.ikale.io/docs/cli/detect/) 和[官方订阅采样渠道](https://lm.ikale.io/docs/cli/sampling/)。常规检测以三条有效回答组成一轮；`sample` 是参考库采集命令。Usage 仅复用 `fpd sample --channel codex-subscription` 的官方 OAuth 传输，将本次三条完整回答按文档支持的保存轮次格式写入 `detection-input.json`，再调用 `fpd --input detection-input.json --strict --json` 离线评分，不直接把参考采样批次当检测轮次。采样器只使用选定 Usage OAuth 账号的 `CODEX_HOME`，固定请求 `https://chatgpt.com/backend-api/codex/responses`，不转发登录凭据到检测网站。

本期固定 `gpt-6.1-sol`、`low`、3 道原版固定挑战、串行采样，每题最多尝试 1 次。没有分词器探测、自动重试或其他模型回退。测试消耗该账号的官方订阅额度，不影响交互会话，不注入 Bark 或性能网关。参考库排名不构成模型身份认证。

采样前必须确认本地 LMFPD 参考库包含 `gpt-6.1-sol`。缺少目标模型或无法读取、识别参考库时，直接跳过，不启动采样进程、不读取订阅登录凭据、不产生新的测试记录。Provider 显示“暂不测试”和具体原因；旧结果标记为“上次结果”并保留历史。参考库检查按文件修改时间和大小缓存，更新本地库后自动重新判断。自定义 `--fpd` 必须能定位到有效的 `lmfpd` npm 包及其 `data/unified_bank.json`，无法确认的包装脚本不执行。

模型匹配同时检查参考条目的 ID 与上游明确列出的展示标签。当前本地库的 `id=gpt-6-astra` 对应 `display_name=gpt-6-astra/6.1-sol`，因此 `gpt-6.1-sol` 已被这个合并参考组覆盖，不应提示“未收录”。斜杠后的短标签仅补全同一模型家族前缀后精确比较，不用子串或相似名称匹配；详情显示实际参考组。合并参考组不能单独区分组内的两个名称，也不构成官方模型等价声明。

## 安装与运行

需要 Node.js 22+；当前已核对版本为 `lmfpd@0.0.1792852997176`。只安装本地引擎，不在运行时执行 `npx latest`：

```sh
# 默认数据根目录；设置了 USAGE_CODEX_DIR / XDG_DATA_HOME 时请使用实际根目录。
npm install --prefix "$HOME/.local/share/usage/codex/fingerprint/tools" \
  --ignore-scripts --no-audit --no-fund lmfpd@0.0.1792852997176

usage codex list
usage codex fingerprint run <OAuth账号名称或ID>
usage codex fingerprint status <OAuth账号名称或ID>
usage codex fingerprint status <OAuth账号名称或ID> --json
```

优先查找数据目录 `fingerprint/tools/node_modules/.bin/fpd`，其次使用 PATH 中的 `fpd`；也可为 `run` / `enable` 指定 `--fpd /绝对路径/fpd`。进程不继承 `API_KEY`、`BASE_URL`、`MODEL` 等推理连接覆盖值。BYOK 账号会在发出请求前拒绝。

## 引擎版本提示

Usage 打开期间在后台查询 npm 的 `lmfpd/latest` 公开元数据，与实际 fpd 安装目录的 `package.json` 版本按 SemVer 比较。成功结果缓存 6 小时，失败后 1 小时再试，并保留上次成功结果；请求最多等待 8 秒，独立于采样任务。查询不携带订阅凭据，不调用检测网站，也不自动安装或更新。关闭 Usage 后停止检查，不创建系统后台任务。

发现更新后，对应 Provider 的订阅测试行提示“lmfpd 有更新”，在该 Provider 上按 `t` 查看当前版本、最新版本与上次成功检查时间；`fingerprint status` 的文本输出也展示已缓存的更新提示，JSON 报告保持向后兼容。升级后提示自动消失。无法识别版本的自定义 fpd 包装脚本不猜测其版本。

默认安装目录可用以下命令手动更新，将版本号替换为提示中的最新版本；自定义 `--fpd` 安装请更新对应位置：

```sh
npm install --prefix "$HOME/.local/share/usage/codex/fingerprint/tools" \
  --ignore-scripts --no-audit --no-fund 'lmfpd@<最新版本>'
```

缓存位于 `<Usage账号数据根目录>/fingerprint/update.json`，删除后下次打开 Usage 会重新查询。

## 每 5 小时任务

```sh
usage codex fingerprint enable <OAuth账号名称或ID>
usage codex fingerprint disable <OAuth账号名称或ID>
```

定时调度属于 Usage 应用进程，打开 Dashboard 或独立账号管理界面时启动。应用入口统一管理后台任务，每 30 秒检查已启用账号是否到期，最多并发测试 2 个账号，同一账号至少间隔 5 小时；切换页面、锁屏不影响调度。关闭 Usage 时取消调度并停止、回收正在采样的子进程，保存已中断的记录。电脑睡眠时暂停，重新打开或唤醒后只检查一次到期，不补跑积压轮次。

不注册系统后台任务，不复制 Usage 二进制。启用配置保存到 `fingerprint/accounts/<账号ID>/schedule.json`，仅包含本地 fpd 路径；可在不同平台使用。配置变化最多 30 秒内被正在运行的 Usage 发现。移动 fpd 后重新运行 `enable`。`disable` 停止之后的自动测试，保留历史记录。手动 `run` 可立即测试，`run --due` 遵守 5 小时间隔；并发任务使用每账号文件锁避免重复。

订阅测试展示在首页对应的 **Provider** 中，按托管账号 ID 关联；同名账号不会合并。仅 GPT 官方 OAuth 订阅显示此能力，BYOK、WorkBuddy 等服务不会混入其他账号的测试结果。Provider 摘要显示此账号的测试结论和引擎更新提示；按 `t` 查看失败原因（包括额度恢复时间）及最近 24 次测试的符号历史，按 `t` 或 Esc 返回。左右切换 Provider 时详情关闭，后续查看使用新账号的记录。历史结果和定时测试配置保留原来的账号绑定，无需迁移。

厂商状态看板只显示官方状态页的三条服务状态与事件历史。每个测试符号代表此 Provider 的一次本机测试，旧记录在左；完成、采样中、限流／部分结果、失败、未测试／中断使用不同符号和文字。不会把排名结果当作服务健康度或模型身份认证。首页可见时每 5 秒在后台读取状态快照，通过 Dashboard 的 AppEvent 与代次校验更新，不触发推理请求。历史摘要按最近报告与运行目录变更失效缓存，空闲刷新不重复解析全部历史；单个账号配置或报告损坏仅影响该账号，其他账号继续测试和展示。

首页摘要显示结果、最接近候选和库内置信度；摘要与详情均不展示测试时间、采样计数和调度说明，这些信息可通过 CLI 与 JSON 报告查询。详情不重复展示完整报告说明。每次仍只采样三条回答，组成一个检测轮次，不需要增加为九条。输入保留原始 `expected_count`、条件与完成状态，拒绝重复挑战、返回模型不符、截断或完整性未知的回答，严格评分由原版 LMFPD 执行。只有引擎报告 `probability_status=reference_calibrated` 且该轮三条回答均有效时才展示其第一候选概率；不自行估算置信度，也不将多轮概率合并。旧的部分评分仍按实际情况展示。参考库限制只提示一次，失败原因与额度恢复时间仍完整展示；详细计数与原始说明保留在 CLI 和 JSON 报告中。Usage 读取 `rounds[].analysis`，兼容旧的单轮 `analysis`，不重复累计单轮别名。

OAuth 登录由官方 Codex 管理；采样器只读取当前登录，不自行刷新或保存 Token。登录过期、网络故障、限流、返回模型不符等情况记录为失败，本轮不重试；需要时通过 Usage 的官方登录入口重新登录，或让官方 Codex 正常刷新登录文件。采样进程最长 15 分钟，离线评分最长 60 秒，输出最多 4 MiB；失败不阻塞 Usage 的启动或首页。

## 记录与排查

每账号结果位于 `<Usage账号数据根目录>/fingerprint/accounts/<账号ID>/`：

- `latest.json`：最近一次尝试的时间、状态、有效样本数、评分轮数与完整轮数、参考组及说明；只有单轮报告才展示最近候选与引擎已校准的置信度。
- `runs/<测试ID>/report.json`：该轮摘要。
- `runs/<测试ID>/detection.json`：原版检测器的离线评分报告。
- `runs/<测试ID>/detection-input.json`：一次检测轮次的三条原始回答及完成证据；由 `samples/result.json` 转换，不改写原始采样批次。
- `runs/<测试ID>/detection-round.json`：旧版拆分轮次修复时新生成的完整离线评分；旧 `detection.json` 保持不变。
- `runs/<测试ID>/samples/`：原版采样清单、每题尝试、原始响应证据和 `result.json`，用于追查 HTTP 错误、数字不足、模型名不符等失败。

保留最近 168 次尝试，按每 5 小时一次约覆盖 35 天。状态和报告以私有权限保存；采样不会写入或替换 OAuth 登录文件。结构化日志事件为 `fingerprint_started`、`fingerprint_finished`、`fingerprint_cleanup_failed`，记录账号 ID、固定模型、结果和有效样本数，不记录凭据或样本正文。失败退出码为 1；失败的详细上游证据保留在本地采样目录。

旧版仅解析顶层 `analysis`，可能把已有有效多轮结果误报为“本轮有效样本不足，无法评分”。可离线修复指定账号的这类摘要：

```sh
usage codex fingerprint repair <OAuth账号名称或ID>
```

修复不会重新采样、请求订阅 API 或改动调度时间。旧版多轮解析错误从 `detection.json` 修复，原摘要先备份为 `report.before-multiround-fix.json` / `latest.before-multiround-fix.json`；未识别合并标签而误报未收录的摘要使用 `before-reference-fix` 后缀。已保存三条完整回答但拆成三轮的旧结果，使用本地 fpd 按一个标准检测轮次严格重算，生成 `detection-round.json`，摘要备份使用 `before-round-fix` 后缀；没有可用 fpd 或完整样本时保留原结果。旧评分和原始采样证据均保持不变。真实 HTTP 失败、其他失败原因均保留。账号正在测试时拒绝修复，以免与正在写入的结果冲突；再次执行不会重复修改已修复记录。

当前安装版本通过 `gpt-6-astra/6.1-sol` 合并参考组覆盖目标型号。完整结果表示本次回答与这个参考组最接近，不证明上游身份，也不能区分该组内两个名称。不会自动把待测样本加入参考库，以免结果被自身样本污染。更新引擎后，仍按本地参考库实际覆盖情况判断是否允许采样。

## 实现边界

`fingerprint::FingerprintService` 是 CLI、后台调度和首页共同使用的应用服务。`codex::CodexSubscriptionDetector` 只负责官方订阅账号的租约、身份校验与 LMFPD 子进程协议；`store::Store` 负责启用配置、报告读取和历史缓存；`runtime::Runtime` 只负责有界调度与退出取消。交互式 Usage 统一由 `AppRuntime` 持有后台任务，命令行只读查询和单次输出不启动自动测试。首页复用既有 AppEvent 与代次校验，测试状态使用独立枚举，符号样式与官方可用性一致但不混用服务健康状态。保留原有 JSON 报告状态值，未识别的新状态显示为未知。
