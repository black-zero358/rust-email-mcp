# 读取能力实施记录

R00–R13 已完成实现及离线验收，随 0.2.2 交付，0.3.0 继续修复兼容性与边界行为。本文保留工作包索引；使用方法见[能力说明](capabilities.md)，后续任务统一见[待闭环事项](../CONTRIBUTING.md#待闭环事项)。

## 实施状态

| 工作包 | 交付内容 | 合并与验收 |
| --- | --- | --- |
| R00–R01 | 附件交付选型、合成探针、Unicode 输入与错误预算 | [PR #63](https://github.com/black-zero358/rust-email-mcp/pull/63) |
| R02–R03 | 读取输出 Schema、账户能力、HTML 可读文本 | [PR #64](https://github.com/black-zero358/rust-email-mcp/pull/64) |
| R04 | Rust 附件下载与离线客户端文件解析 | [PR #65](https://github.com/black-zero358/rust-email-mcp/pull/65) |
| R05 | 多账户只读、身份与共享调度 | [多账户设计](multi-account-design.md) |
| R06–R07 | 有界 BODYSTRUCTURE、附件清单、部件下载及正文读取 | [架构](architecture.md#mcp-and-mail) |
| R08 | 正文预览与邮件头分类 | [能力说明](capabilities.md#筛选与分页) |
| R09 | 跨文件夹会话与精确 Message-ID 定位 | [会话契约](capabilities.md#跨文件夹会话) |
| R10 | 持久元数据与会话游标 | [缓存设计](cache-design.md) |
| R11 | 完整离线正文、附件、搜索与写入失效 | [内容缓存设计](cache-content-design.md) |
| R12 | 全量回填、后台同步、CONDSTORE/QRESYNC | [同步设计](cache-sync-design.md) |
| R13 | 双账户持续负载、资源采样与提交绑定的发布候选 | [离线验收](archive/2026-11/validation.md#2026-11-05-r13-整体验收与-022-候选) |

附件内容验收使用合成 CSV/XLSX/PDF。真实 ChatGPT 文件消费及服务商兼容验收列入待闭环事项。2026-11-06 已精简测试设施，当前回归入口见[验证表](architecture.md#invariants-and-verification)。

原始目标、工作包细节、逐提交结果和接口研究保留在[历史方案](archive/2026-11/read-capabilities-plan.md)。
