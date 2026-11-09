# 历史文档归档

本目录保存已完成工作包的设计过程、版本化实验和审阅证据。每项记录保留原日期、提交和结果；复现时使用记录中的源码版本。现行说明从[文档导航](../README.md)进入。

| 文档 | 用途 |
| --- | --- |
| [2026-11 验证](2026-11/validation.md) | R00–R13、离线附件、缓存与同步、测试精简 |
| [2026-10 验证](2026-10/validation.md) | 早期部署、资源测量及协议修复；含 #28–#34 详细证据入口 |
| [Issue 修复台账](issue-campaigns.md) | 2026-10-29 和 2026-11-06 已完成修复的 PR、审阅与 CI |
| [读取能力原始方案](2026-11/read-capabilities-plan.md) | 原始需求、分批依赖及逐提交实施过程 |
| [附件交付探针](2026-11/attachment-delivery-probe.md) | 2026-11-02 的能力盘点和合成文件实验 |
| [附件客户端研究](2026-11/attachment-client-adapter-design.md) | 离线 Rust/Node 联调及当时的宿主接口候选 |

Python/Node 附件原型和 R13 长跑驱动已于 2026-11-06 精简。原型源码见[精简前提交](https://github.com/black-zero358/rust-email-mcp/tree/42873c0f2ea9f8ff284fa044c4111d7cb97f8dc9)；合成夹具、JSON 证据和当前 Rust 回归继续保留。发布校验和与现行复验入口见[验证记录](../validation.md)。
