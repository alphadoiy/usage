# 官方订阅模型指纹测试

参考 [LM Fingerpoint Detector 文档](https://lm.ikale.io/docs/cli/sampling/) 和其官方订阅采样渠道。Usage 调用本地 `fpd sample --channel codex-subscription` 采样，再用 `fpd --input` 离线评分，不转发登录凭据到检测网站。采样器只使用选定 Usage OAuth 账号的 `CODEX_HOME`，固定请求 `https://chatgpt.com/backend-api/codex/responses`。

本期固定 `gpt-6.1-sol`、`low`、3 道原版固定挑战、串行采样，每题最多尝试 1 次。没有分词器探测、自动重试或其他模型回退。测试消耗该账号的官方订阅额度，不影响交互会话，不注入 Bark 或性能网关。参考库排名不构成模型身份认证；库中没有 `gpt-6.1-sol` 时明确提示“不能据此判断换模”，仍保留原始样本。

## 安装与运行

需要 Node.js 22+；已核对版本 `lmfpd@0.0.1792242250521`，对应上游提交 `50a364bc686279094265f34f18f5501eac1d73f4`。只安装本地引擎，不在运行时执行 `npx latest`：

```sh
# 默认数据根目录；设置了 USAGE_CODEX_DIR / XDG_DATA_HOME 时请使用实际根目录。
npm install --prefix "$HOME/.local/share/usage/codex/fingerprint/tools" \
  --ignore-scripts --no-audit --no-fund lmfpd@0.0.1792242250521

usage codex list
usage codex fingerprint run <OAuth账号名称或ID>
usage codex fingerprint status <OAuth账号名称或ID>
usage codex fingerprint status <OAuth账号名称或ID> --json
```

优先查找数据目录 `fingerprint/tools/node_modules/.bin/fpd`，其次使用 PATH 中的 `fpd`；也可为 `run` / `enable` 指定 `--fpd /绝对路径/fpd`。进程不继承 `API_KEY`、`BASE_URL`、`MODEL` 等推理连接覆盖值。BYOK 账号会在发出请求前拒绝。

## 引擎版本提示

Usage 打开期间在后台查询 npm 的 `lmfpd/latest` 公开元数据，与实际 fpd 安装目录的 `package.json` 版本按 SemVer 比较。成功结果缓存 6 小时，失败后 1 小时再试，并保留上次成功结果；请求最多等待 8 秒，独立于采样任务。查询不携带订阅凭据，不调用检测网站，也不自动安装或更新。关闭 Usage 后停止检查，不创建系统后台任务。

发现更新后，首页订阅测试行提示“lmfpd 有更新”，按 Enter 查看当前版本、最新版本与上次成功检查时间；`fingerprint status` 的文本输出也展示已缓存的更新提示，JSON 报告格式保持不变。升级后提示自动消失。无法识别版本的自定义 fpd 包装脚本不猜测其版本。

默认安装目录可用以下命令手动更新，将版本号替换为提示中的最新版本；自定义 `--fpd` 安装请更新对应位置：

```sh
npm install --prefix "$HOME/.local/share/usage/codex/fingerprint/tools" \
  --ignore-scripts --no-audit --no-fund 'lmfpd@<最新版本>'
```

缓存位于 `<Usage账号数据根目录>/fingerprint/update.json`，删除后下次打开 Usage 会重新查询。

## 每小时任务

```sh
usage codex fingerprint enable <OAuth账号名称或ID>
usage codex fingerprint disable <OAuth账号名称或ID>
```

定时调度属于 Usage 应用进程，打开 Dashboard 或独立账号管理界面时启动。应用入口统一管理后台任务，每 30 秒检查已启用账号是否到期，最多并发测试 2 个账号，同一账号至少间隔一小时；切换页面、锁屏不影响调度。关闭 Usage 时取消调度并停止、回收正在采样的子进程，保存已中断的记录。电脑睡眠时暂停，重新打开或唤醒后只检查一次到期，不补跑积压轮次。

不注册系统后台任务，不复制 Usage 二进制。启用配置保存到 `fingerprint/accounts/<账号ID>/schedule.json`，仅包含本地 fpd 路径；可在不同平台使用。配置变化最多 30 秒内被正在运行的 Usage 发现。移动 fpd 后重新运行 `enable`。`disable` 停止之后的自动测试，保留历史记录。手动 `run` 可立即测试，`run --due` 遵守一小时间隔；并发任务使用每账号文件锁避免重复。

首页“厂商状态 · 订阅测试”看板保留官方状态页的三条服务状态，另列“GPT 订阅测试”本机状态。选择后按 Enter 查看最后测试时间、有效样本数、失败原因（包括额度恢复时间）及最近 24 次测试的符号历史；每个符号代表一次真实测试，旧记录在左。完成、采样中、限流／部分结果、失败、未测试／中断使用不同符号和文字，不仅依赖颜色。官方状态页与本机测试明确区分；不会把排名结果当作服务健康度或模型身份认证。首页可见时每 5 秒在后台读取状态快照，通过 Dashboard 的 AppEvent 更新，不触发推理请求。历史报告不可变，按最近报告与运行目录变更失效缓存，空闲刷新不重复解析全部历史；单个账号配置或报告损坏仅影响该账号，其他账号继续测试和展示。

OAuth 登录由官方 Codex 管理；采样器只读取当前登录，不自行刷新或保存 Token。登录过期、网络故障、限流、返回模型不符等情况记录为失败，本轮不重试；需要时通过 Usage 的官方登录入口重新登录，或让官方 Codex 正常刷新登录文件。采样进程最长 15 分钟，离线评分最长 60 秒，输出最多 4 MiB；失败不阻塞 Usage 的启动或首页。

## 记录与排查

每账号结果位于 `<Usage账号数据根目录>/fingerprint/accounts/<账号ID>/`：

- `latest.json`：最近一次尝试的时间、状态、有效样本数、参考库是否包含目标模型、最接近候选及说明。
- `runs/<测试ID>/report.json`：该轮摘要。
- `runs/<测试ID>/detection.json`：原版检测器的离线评分报告。
- `runs/<测试ID>/samples/`：原版采样清单、每题尝试、原始响应证据和 `result.json`，用于追查 HTTP 错误、数字不足、模型名不符等失败。

保留最近 168 次尝试，约一周的每小时历史。状态和报告以私有权限保存；采样不会写入或替换 OAuth 登录文件。结构化日志事件为 `fingerprint_started`、`fingerprint_finished`、`fingerprint_cleanup_failed`，记录账号 ID、固定模型、结果和有效样本数，不记录凭据或样本正文。失败退出码为 1；失败的详细上游证据保留在本地采样目录。

截至接入时，上游参考库尚无 `gpt-6.1-sol`。因此目前适合持续积累这个官方模型的样本及观察排名变化，不应把其他型号排名第一解释成账号提供了其他模型。不会自动把待测样本加入参考库，以免结果被自身样本污染。

## 实现边界

`fingerprint::FingerprintService` 是 CLI、后台调度和首页共同使用的应用服务。`codex::CodexSubscriptionDetector` 只负责官方订阅账号的租约、身份校验与 LMFPD 子进程协议；`store::Store` 负责启用配置、报告读取和历史缓存；`runtime::Runtime` 只负责有界调度与退出取消。交互式 Usage 统一由 `AppRuntime` 持有后台任务，命令行只读查询和单次输出不启动自动测试。首页复用既有 AppEvent 与代次校验，测试状态使用独立枚举，符号样式与官方可用性一致但不混用服务健康状态。保留原有 JSON 报告状态值，未识别的新状态显示为未知。
