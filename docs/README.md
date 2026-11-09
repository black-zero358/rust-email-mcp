# 文档导航

当前源码版本为 **0.3.0**，提供 Linux amd64 单节点部署、Streamable HTTP 和 OAuth/OIDC 认证。安装入口见[项目 README](../README.md)，发布产物见 [v0.3.0](https://github.com/black-zero358/rust-email-mcp/releases/tag/v0.3.0)。

## 使用与维护

| 需要做什么 | 文档 |
| --- | --- |
| 安装、配置认证、多账户、缓存、备份与恢复 | [部署指南](deployment.md) |
| 查询邮件、分页、会话、附件和离线读取 | [能力与工具契约](capabilities.md) |
| 排查连接、登录和令牌刷新 | [接入排障](troubleshooting.md) |
| 升级、回退、发布、依赖审计 | [维护规程](maintenance.md) |
| 开发、验证、PR 与待办 | [贡献指南](../CONTRIBUTING.md) · [代理指南](../AGENTS.md) |
| 私密报告漏洞 | [安全政策](../SECURITY.md) |

## 当前设计

| 主题 | 文档 |
| --- | --- |
| 模块边界、协议、发送生命周期与回归映射 | [架构](architecture.md) |
| 账户配置、授权、注册库与共享调度 | [多账户设计](multi-account-design.md) |
| 配置、元数据、游标与恢复 | [持久缓存设计](cache-design.md) |
| 原件、正文、附件、离线读取与失效 | [内容缓存设计](cache-content-design.md) |
| 全量回填、增量同步、配额与协议加速 | [同步设计](cache-sync-design.md) |

## 方案与记录

| 状态 | 文档 |
| --- | --- |
| 待实施 | [HTTP 本地分发方案](http-distribution-plan.md)：npm/PyPI 安装、本地初始化和服务生命周期、多平台存储 |
| 已完成 | [读取能力实施记录](read-capabilities-plan.md) · [Issue 修复记录](../ISSUE_PROGRESS.md) |
| 发布验收与复验入口 | [验证记录](validation.md) |
| 历史实验、阶段方案与旧接口研究 | [归档索引](archive/README.md) |

维护时更新主题所属文档，再同步必要的入口摘要。未来命令放在待实施方案，历史命令和测试计数放在归档；现行使用说明只列已实现入口。
