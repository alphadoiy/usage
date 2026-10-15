# 第一阶段架构实验

`provider_contracts.rs` 由 `../ablate-provider-stage1.py` 注入临时源码副本，验证两个 Home 的真实查询结果不会串用缓存。它不改写工作区生产代码。

`ui_probes.rs`、`hub_probes.rs` 保存优化前的四个失败探针，属于历史实验输入，使用当时的接口。原始失败日志、源码快照及脚本副本保留在 `/private/tmp/usage-stage1-ablation-20261015`。当前 runner 不再注入这两个历史文件。

四项契约已迁入正常 Cargo 回归：`src/ui/app.rs::account_regression_tests` 与 `src/hub.rs::regression_tests`。当前 runner 在完整变体测试后用 `regression_tests` 筛选运行这些契约；它们应当全部通过。主动重置的账号定位另有正式回归。

实验方法和优化前结果见 [评估报告](../../docs/provider-stage1-ablation.md)。
