# 第三阶段修复与优化（2026-10-16）

本轮落实 [第三阶段消融](provider-stage3-ablation.md) 发现的解析缺口、回归覆盖缺口和两项结构优化。历史实验 JSON 保持原样，不将旧快照的失败改写为通过。

## 行为修复

启动身份解析改为跟踪根命令、`exec` 子命令和提示词三个位置。根命令 `exec` 及其别名 `e` 后的 `resume`、`fork`、`review`、`help` 不再被当作普通提示词；这些配置来源尚未支持的形式记录为 `unresolved`，不产生已匹配资源，`resources --run` 明确拒绝查询。原有 Agent 启动仍可继续。

例如 `usage codex run Work -- exec resume <UUID>` 以前会记录 `resolved`，现在记录 `unresolved`。模型参数和其他已支持选项可以出现在命令之间，其值即使是 `resume` 或 `review` 也不会被当成子命令。

提示词按所在命令层级处理：`exec "resume the migration"`、`e "review the migration"`、`exec login`、`exec exec` 仍是普通提示词；`-- resume` 和 `exec -- resume` 保持明确的提示词边界。没有采用扫描任意关键词的规则，也没有宣称实现完整的 Codex 参数解析器。

## 结构优化

- `resources_for_run` 使用一次 LEFT JOIN 读取 `profile_id` 与私有上下文，反序列化一次。保留未知运行、旧运行缺少上下文、配置失效和凭据轮换时的拒绝行为。
- 启动捕获用局部 `OnceCell` 延迟创建资源服务及 HTTP 客户端；没有候选资源时无需构造。仍调用真正的资源工厂校验，不用服务名称检查替代它。
- 修正模拟 HTTP 服务的接受 socket 模式：显式切回阻塞读取，保留原读取超时，排除 macOS 上偶发的 `WouldBlock` 干扰。修改仅在测试夹具。

## 回归覆盖

11 项实验探针迁入正常 Cargo 测试，并增强嵌套命令与提示词用例：

- [运行身份集成测试](../tests/run_identity.rs)：`exec` / `e` 下的恢复、分叉和审查；选项值与提示词边界；推理配置改变；移除账号关联；未知资源工厂；Context 插入失败不留下孤立 Run。
- [关联单元测试](../src/runs.rs)：查询 resolver 保留已验证凭据并拒绝无关引用；凭据证明不能跨 Run 复用；拒绝身份或服务元数据内部不一致。

嵌套命令集成测试除了检查状态，也通过真实 CLI 检查 `resources --run` 拒绝，模拟子进程不会访问真实 Codex 或中转站。未把“一次启动必须打开两个 Manager”的计数探针迁入回归，以免固化已知的重复工作。

## 验证

- 全量测试：205 项库测试、24 项 Codex CLI、5 项资源 CLI、7 项资源查询、12 项运行身份，共 **253 项通过**；管理存储隔离于 `/private/tmp/usage-stage3-fix-20261016`。
- `cargo fmt --all -- --check`、`cargo clippy --offline --all-targets -- -D warnings`、`git diff --check` 通过。
- 当前消融脚本 23 个变体的修改锚点全部通过检查。两项优化现已成为基线，脚本通过反向恢复重复读取与提前构造做对照；嵌套命令变体改为撤销新的门控。
- `--audit` 现在筛选已迁入生产回归的 `run_binding_` 测试，不再追加历史探针，以免测试重复或过期。

定向消融只运行正常回归中的 11 项 `run_binding_` 测试。基线全部通过；撤销嵌套命令门控及六项保护后，对应回归均失败。没有重跑修复前的整个消融矩阵。

| 变体 | 通过 / 失败 |
| --- | --- |
| 完整回归子集 | 11 / 0 |
| 撤销嵌套命令门控 | 9 / 2 |
| 推理配置不参与版本摘要 | 10 / 1 |
| 移除账号关联复核 | 10 / 1 |
| 查询凭据退回环境读取 | 10 / 1 |
| 忽略资源工厂验证 | 10 / 1 |
| 移除记录共同事务 | 10 / 1 |
| 移除服务一致性检查 | 10 / 1 |

机器结果：[provider-stage3-optimization-results.json](provider-stage3-optimization-results.json)。逐变体源码差异、日志和本轮快照保存在 `/private/tmp/usage-stage3-fix-regressions-20261016`。

## 保留的边界

子进程环境凭据固定及其余关联校验保留。启动身份仍是受支持配置来源的本地推断；原生 profile、项目配置层、OAuth、恢复会话和交互内切换仍需 Agent 确认后才能进一步支持。未测真实 API、Apple Watch 投递或真实启动延迟。

本轮没有调整公开 `RunContext` 或数据库快照结构。平行数组的内部表示、完整配置加载与数据目录发现的耦合，以及重复打开 Manager，仍可在后续应用服务迁移中收敛。
