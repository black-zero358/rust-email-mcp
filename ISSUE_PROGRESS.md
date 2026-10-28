# Issue 修复进度（2026-10-29）

范围：启动时 GitHub 全部开放 issue #24–#34。主代理统筹；实施与独立审核均使用 gpt-6.1-sol / medium，fork_turns=none。

每项依次完成：当前代码核实 → 修复与合成回归 → 独立 PR → 独立子代理审核 → 最终 head 的 check/audit/container 成功 → GitHub 合并 main。

本轮只进行源码修复、合成验证、本地构建和 CI，不执行生产部署或真实邮件操作。

完成：11 项均核实、修复、独立审阅并在最终 head 的三项 CI 成功后合并。最终集成源码为 [`e649572`](https://github.com/black-zero358/rust-email-mcp/commit/e6495729cef5bf2b7303de91e74e19ead3bdcdc8)；2026-10-29 核对 GitHub 开放 issue 列表为空。审阅及 CI 链接绑定各 PR 的最终 head，合并链接指向实际 main 提交。

| Issue | 事项 | 逐项评估与回归证据 | PR | 最终 head 与独立审核 | 合并与 CI |
| --- | --- | --- | --- | --- | --- |
| [#24](https://github.com/black-zero358/rust-email-mcp/issues/24) | IMAP 预分配预算在部分响应情况下未被强制执行 | 启动基线仍存在：真实 parser 扩容未受应用预算约束；小预算容量、分片及完整响应回归通过。 | [#39](https://github.com/black-zero358/rust-email-mcp/pull/39) | 通过，[5458ed9](https://github.com/black-zero358/rust-email-mcp/commit/5458ed9c71320d8c0dbb40407d62d3e7fc144265) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/39#pullrequestreview-5686560128) | 已合并 [9bb9721](https://github.com/black-zero358/rust-email-mcp/commit/9bb9721e86e14ca24bd471fb82bc94aa27a1c888)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40731228840) |
| [#25](https://github.com/black-zero358/rust-email-mcp/issues/25) | multipart/alternative 的纯文本探测会选中不完整表示，读取和转发可能只剩签名 | 启动基线仍存在：纯文本探测跳过 HTML 后误选签名；原读取/转发回归失败，修后保留完整表示。 | [#37](https://github.com/black-zero358/rust-email-mcp/pull/37) | 通过，[c804dde](https://github.com/black-zero358/rust-email-mcp/commit/c804dde64d613c5ee205322cfe2e9d0f246bcc86) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/37#pullrequestreview-5686385868) | 已合并 [18b26dd](https://github.com/black-zero358/rust-email-mcp/commit/18b26ddc8a365ecdabb7a38b50a476b8f9155679)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40728896551) |
| [#26](https://github.com/black-zero358/rust-email-mcp/issues/26) | 未识别的 Content-Disposition 未按附件处理，正文与转发仍可能包含附件内容 | 启动基线仍存在：显式未知 disposition 未作为附件；公开分类回归原失败，修后分类及共享索引回归通过。 | [#40](https://github.com/black-zero358/rust-email-mcp/pull/40) | 通过，[86b978b](https://github.com/black-zero358/rust-email-mcp/commit/86b978b04dfd17b7f0307f6ee361cf6639997d47) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/40#pullrequestreview-5686664754) | 已合并 [d9527ed](https://github.com/black-zero358/rust-email-mcp/commit/d9527ed3341bd6cf6b065a4644b86edb3f52e4ff)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40732410674) |
| [#27](https://github.com/black-zero358/rust-email-mcp/issues/27) | get_email 的 128 KiB 正文截断与完整响应预算冲突，普通长邮件无法返回截断前缀 | 启动基线仍存在：128 KiB 正文与双份 JSON 预算冲突；合成长邮件原超限，修后真实线路预算与 UTF-8 前缀回归通过。 | [#38](https://github.com/black-zero358/rust-email-mcp/pull/38) | 通过，[aac05a7](https://github.com/black-zero358/rust-email-mcp/commit/aac05a738218c594a66d118c9171979ddcd2dcfb) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/38#pullrequestreview-5686443004) | 已合并 [717a302](https://github.com/black-zero358/rust-email-mcp/commit/717a302e3ca955a084c3a0cca78d38dcb0fc141a)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40729971052) |
| [#28](https://github.com/black-zero358/rust-email-mcp/issues/28) | EXAMINE 缺少 tagged completion 时返回成功，断连会被报告为空邮箱 | 启动基线仍存在：EXAMINE 未等匹配 tagged completion 即成功；空/部分结果与 OK、NO、BAD、EOF、错误 tag 矩阵回归通过。 | [#41](https://github.com/black-zero358/rust-email-mcp/pull/41) | 通过，[3682cb7](https://github.com/black-zero358/rust-email-mcp/commit/3682cb71d404dea45a0adc7b5c526a838c49afaf) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/41#pullrequestreview-5686923800) | 已合并 [3b8e8b0](https://github.com/black-zero358/rust-email-mcp/commit/3b8e8b06e34a35a7741254f77acc6b8cf8bbbf1a)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40735674497) |
| [#29](https://github.com/black-zero358/rust-email-mcp/issues/29) | 同 UID 的异步 FLAGS 响应会导致读取失败、列表重复和突破 limit | 启动基线仍存在：首条 UID 响应遮住所需属性，列表逐响应建摘要；原读取失败、limit=1 返回5条，修后属性归并/唯一 UID 回归通过。 | [#42](https://github.com/black-zero358/rust-email-mcp/pull/42) | 通过，[29fb904](https://github.com/black-zero358/rust-email-mcp/commit/29fb90427f9c6a0c79bf35792b308530b882d071) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/42#pullrequestreview-5686664431) | 已合并 [ca9eeac](https://github.com/black-zero358/rust-email-mcp/commit/ca9eeac23f9dfe3c3d0f3a95680ee896a7829ec1)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40732406435) |
| [#30](https://github.com/black-zero358/rust-email-mcp/issues/30) | APPEND 等待 literal continuation 时将合法服务器通知当作失败 | 启动基线仍存在：APPEND 只读一次响应，合法通知被拒；原通知回归失败，修后等待 Continue 且拒绝后不发 literal。 | [#44](https://github.com/black-zero358/rust-email-mcp/pull/44) | 通过，[1e6adc2](https://github.com/black-zero358/rust-email-mcp/commit/1e6adc21d1af6b4dc5222772f869b118c18a557f) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/44#pullrequestreview-5686793070) | 已合并 [11b2164](https://github.com/black-zero358/rust-email-mcp/commit/11b21644c773a7a26afa31a186572fe6d374b9da)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40734201929) |
| [#31](https://github.com/black-zero358/rust-email-mcp/issues/31) | get_emails 将单封邮件的本地 MIME 错误当成连接失败，跳过后续正常邮件 | 启动基线仍存在：本地 MIME 错误触发连接级停止；原 completed=1，修后坏内容后正常项继续，坏连接仍停止。 | [#45](https://github.com/black-zero358/rust-email-mcp/pull/45) | 通过，[83f8cb9](https://github.com/black-zero358/rust-email-mcp/commit/83f8cb9ce26f64faf5b0f04f30d9f440bd5ce1cf) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/45#pullrequestreview-5686766177) | 已合并 [c80e7b5](https://github.com/black-zero358/rust-email-mcp/commit/c80e7b5bb312037dd520a736d1a0d439e5e3591a)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40733769325) |
| [#32](https://github.com/black-zero358/rust-email-mcp/issues/32) | ASCII 信封下保留的原始 UTF-8 草稿头未触发 SMTPUTF8 协商 | 启动基线仍存在：仅按信封判断而漏掉原始国际化头；原线路缺 SMTPUTF8，修后能力矩阵与 journal 拒绝回归通过。 | [#46](https://github.com/black-zero358/rust-email-mcp/pull/46) | 通过，[731ce35](https://github.com/black-zero358/rust-email-mcp/commit/731ce3545908581d191f35c02236cd1d9fffe76c) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/46#pullrequestreview-5686820183) | 已合并 [edf5658](https://github.com/black-zero358/rust-email-mcp/commit/edf56589113f0886530d50cda39aea5c0f53af58)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40734268050) |
| [#33](https://github.com/black-zero358/rust-email-mcp/issues/33) | LF 草稿正文被接受后直接提交 SMTP DATA，缺少线路格式校验 | 启动基线仍存在：原始草稿正文未经 DATA 适用性校验；普通 LF 草稿原被接受，修后在 claim 前拒绝，合法 CRLF/编码附件保持。 | [#47](https://github.com/black-zero358/rust-email-mcp/pull/47) | 通过，[239a32f](https://github.com/black-zero358/rust-email-mcp/commit/239a32f47561e05402387ddbacc5408f80b7f4e6) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/47#pullrequestreview-5686929710) | 已合并 [e649572](https://github.com/black-zero358/rust-email-mcp/commit/e6495729cef5bf2b7303de91e74e19ead3bdcdc8)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40735661239) |
| [#34](https://github.com/black-zero358/rust-email-mcp/issues/34) | 未识别的 Content-Transfer-Encoding 被静默当作明文读取和转发 | 启动基线仍存在：未知 CTE 被默认当作 SevenBit；原不支持编码被伪解码，修后不透明处理、完整回退及下载拒绝回归通过。 | [#43](https://github.com/black-zero358/rust-email-mcp/pull/43) | 通过，[ef91f26](https://github.com/black-zero358/rust-email-mcp/commit/ef91f263f54d7881c4d9a77916cd7d812ec067d0) · [审阅](https://github.com/black-zero358/rust-email-mcp/pull/43#pullrequestreview-5686665105) | 已合并 [49075fc](https://github.com/black-zero358/rust-email-mcp/commit/49075fc1df51b7ecff639c4821f538a579fae24d)；[三项 CI 成功](https://github.com/black-zero358/rust-email-mcp/actions/runs/40732425737) |

## 评估范围与完成清单

本轮在启动源码 `36177076aae667cd72ba3ca81556b94575ab0fc6` 上逐项复核，#24–#34 均确认仍存在；表内是修复前根因与对应合成回归的摘要，完整设计、失败路径和验证边界见各 PR/验证记录。涉及敏感传输边界的完整触发报文不在本表复制。

- [x] 11 项逐个核实、修复并建立独立 PR。
- [x] 11 项最终 head 的独立 COMMENT 审阅与 check/audit/container 全部成功。
- [x] 11 项全部合并 main，核实实际 merge SHA。
- [x] 11 项 issue 已关闭，验证入口、最终审阅、CI 与合并证据已归档。

## 版本与运行边界

源码由 0.1.1 提升至 **0.2.0**：DATA 对裸 LF/CR、NUL、过长行的拒绝，以及国际化头检查的预算收敛，会拒绝此前接受的发送输入，按 0.y.z 兼容政策增加 y。字段和数据库格式未改变，不需要配置/数据库迁移；已有 accepted/unknown 与去重键继续保留。

本轮只有源码修复、合成测试、本地构建与 CI，没有发布新产物、生产部署或真实邮件操作；真实 IMAP/SMTP、持续负载和 musl 发布验收仍须单独执行。

验证总入口见 [docs/validation.md](docs/validation.md)，兼容性与升级边界见 [docs/maintenance.md](docs/maintenance.md)。

## 执行记录

- 初始 main：`36177076aae667cd72ba3ca81556b94575ab0fc6`；工作目录干净，已 fetch。
- 首轮并行：#24 parser 分配预算、#25 alternative 完整性、#27 正文完整响应预算。
- PowerShell PATH 未找到 Cargo；已定位 WSL Ubuntu `/home/oeic_fe/.cargo/bin/cargo`，使用现有分离构建缓存。
- 最终源码：`e6495729cef5bf2b7303de91e74e19ead3bdcdc8`；11 个独立修复 PR 均已合并，GitHub 开放 issue 为 0。本汇总是单独的文档归档，不发布版本或操作生产环境。
