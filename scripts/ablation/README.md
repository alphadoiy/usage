# 架构消融实验

`provider_contracts.rs` 由 `../ablate-provider-stage1.py` 注入临时源码副本，验证两个 Home 的真实查询结果不会串用缓存。它不改写工作区生产代码。

`ui_probes.rs`、`hub_probes.rs` 保存优化前的四个失败探针，属于历史实验输入，使用当时的接口。原始失败日志、源码快照及脚本副本保留在 `/private/tmp/usage-stage1-ablation-20261015`。当前 runner 不再注入这两个历史文件。

四项契约已迁入正常 Cargo 回归：`src/ui/app/account_regression_tests.rs` 与 `src/hub.rs::regression_tests`。当前 runner 在完整变体测试后用 `regression_tests` 筛选运行这些契约；它们应当全部通过。主动重置的账号定位另有正式回归。

实验方法和优化前结果见 [评估报告](../../docs/provider-stage1-ablation.md)。

## 公共执行器

`runner.py` 统一两份脚本的源码快照、精确修改锚点、APFS 构建缓存克隆、隔离环境、测试计数、日志保留和恢复。实验使用临时 HOME／XDG／Usage 配置，清除继承的 API 凭据和 Bark／运行上下文；保留 Cargo 与 Rustup 缓存路径。恢复执行要求生产输入与快照相同，重复执行不会覆盖此前日志，编译错误与超时不计为有效消融。

先检查全部变体，无需编译：

```sh
python3 scripts/ablate-provider-stage1.py --output /private/tmp/usage-stage1-anchors --check-anchors
python3 scripts/ablate-run-bindings.py --output /private/tmp/usage-stage3-anchors --check-anchors
python3 -m unittest discover -s scripts/ablation -p 'test_*.py'
```

`--output` 必须是新的临时目录，或使用 `--resume` 恢复已有快照。两份脚本都会在开始构建前检查全部选定变体，并在结束或失败后恢复实验副本。

2026-10-18 更新了生命周期事务和上下文查询的变体锚点。第一阶段的 `reuse_open_manager` 已随生产优化退役，改用 `eager_manager_open` 反向恢复提前打开，以检查重复工作是否会被回归发现。历史 JSON 报告保留当时的变体名称与结果，不改写为当前基线。

2026-10-24 同步托管启动、运行记录及历史增量重构后的锚点。`bypass_hub_launch` 现在绕过 `codex_manager::managed::start` 中的 Hub 接入，保留原请求参数和进程句柄；其余变体继续保持原有单因素含义。日常 CI 检查全部 35 个变体锚点，完整消融仍按需在隔离副本运行。
