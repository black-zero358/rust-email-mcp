# R00 附件交付选型与隔离探针

> 历史归档：保留对应日期和提交的设计、实施与验收记录。现行文档见[文档导航](../../README.md)。 Python/Node 原型及其运行命令已于 2026-11-06 退出主线，源码可在[精简前提交](https://github.com/black-zero358/rust-email-mcp/tree/42873c0f2ea9f8ff284fa044c4111d7cb97f8dc9/scripts)查阅；当前下载契约见[能力说明](../../capabilities.md#附件与兼容性)。

日期：2026-11-02。产品源码基线：`e58090129eda40a47e593a8b89928291559bf1bd`（0.2.1）。对应 [R00/R04 实施方案](read-capabilities-plan.md#r00-与-r04-附件交付)。本次交付确定合成验证方法、能力矩阵及客户端适配归属。按用户随后确认的范围，R04 只做本地离线测试，完成 Rust 下载集成及 CSV/XLSX/PDF 内容读取；ChatGPT 实际连接测试后移。

选型：产品保留同主体 Bearer GET 作为原始字节通道，标准客户端负责下载、核对摘要及本地保存，再由文件解析器读取内容。标准 MCP 资源及 ChatGPT 文件接口保留为后续接入候选；本阶段使用本地模拟邮箱和合成文件验收。

## 已交付的可复现证据

- [探针与运行说明](../../../scripts/attachment-delivery-probe/README.md)：独立 uv 项目、锁定解析器、确定性 CSV/XLSX/PDF 生成器、loopback MCP/Bearer HTTP 服务及客户端。
- [自测结果](../../../scripts/attachment-delivery-probe/evidence/selftest-2026-11-02.json)：8 组 fixture 的原始长度、SHA-256、GET/资源/Base64 结果与解析内容。
- [现场工具清单](../../../scripts/attachment-delivery-probe/evidence/tool-surface-2026-11-02.json)：本任务可见的 QQ 邮箱工具及宿主文件接口范围，未调用真实邮箱。

验收环境为 Windows、CPython 3.13.13、Python 标准库 HTTP 客户端、loopback JSON Streamable HTTP。依赖锁定为 openpyxl 3.1.5、et-xmlfile 2.0.0、pypdf 6.12.0，均只用于开发解析。探针服务使用隔离的一次性凭据，无 IMAP/SMTP、生产 OAuth、部署或邮件写入。

探针自测覆盖的是本目录的合成 HTTP 服务与客户端。现有 Rust handler 的实现和回归入口分别为 [mail/attachments.rs](../../../src/mail/attachments.rs) 的 `download_with`、`http_download_authorization_binary_headers_and_transfer_capacity`；本次没有通过该 handler 运行 fixture。R04 将同一组 fixture 接入 Rust 的隔离 IMAP/HTTP 测试，补产品错误映射、UIDVALIDITY、取消及并发覆盖。

## 能力矩阵

| 客户端/路径 | 实际观察或测试 | 文件消费结果 | R04 下一验收项 |
| --- | --- | --- | --- |
| Python stdlib 探针 0.1.0，HTTP MCP JSON + Bearer GET | initialize、tools/list、tools/call、同主体 GET；8 个 fixture 逐字节及 SHA-256 一致 | CSV 与 XLSX 金额 194，XLSX `明细` 数量 6；PDF 正确提取 CEDAR-729；同名文件分别为 11/29 | 用同一 fixture 验收 Rust handler |
| Python stdlib 探针 0.1.0，MCP resources | `resource_link` → `resources/read` 返回带 MIME 的 Base64 blob；8 个 fixture 字节一致 | 本地解析器成功打开；资源清单可为空，工具返回的 URI 可直接读取 | 目标宿主是否执行 resources/read，以及接收字节/输出预算 |
| 本任务 Codex desktop 的 QQ 邮箱连接 | 清单包含 `download_attachment`，说明为 Base64、128 KiB；未见 list_attachments / prepare_attachment_download；宿主未提供客户端版本 | 已完成工具面盘点；真实附件调用数 0 | 在隔离连接刷新工具后记录实际版本、schema、响应及文件消费 |
| 产品源码 0.2.1 | download_attachment 的解码预算为 `max_output/4`；默认 2 MiB 输出 → 512 KiB；二进制 GET 600 秒/64 句柄/4 槽 | 源码路径和现有回归可定位 | Rust 隔离集成及目标客户端验收 |
| ChatGPT web 自定义远程 MCP | 官方支持添加远程 MCP 与管理工具；本次没有新建/重新登录目标连接 | 待目标会话自动收到文件并计算 fixture 内容 | 记录浏览器/客户端版本、连接方法、宿主文件引用、分析结果 |
| ChatGPT MCP Apps UI 的文件辅助接口 | 官方列出 uploadFile / selectFiles / getFileDownloadUrl；属于可选宿主扩展 | 可作为适配候选；需在目标宿主 feature-detect 并验收文件进入后续分析 | 验证已授权二进制来源如何转换成宿主可接收文件，记录返回 fileId 及分析结果 |
| Pages/Drive/Notion/Pets 等已连接文件工具 | 现场接口有具体资源或类型范围 | 适用各自应用的文件生命周期 | 由目标宿主提供通用接收接口后再选择；本轮不创建无关外部文件 |

现场的 128 KiB 工具描述与源码默认 512 KiB 分别记录。连接描述刷新与部署版本由目标隔离连接核验，避免用其中一项替另一项填写验收结果。

## Fixture 与判定

所有字节由 `probe.py fixtures()` 确定性生成，ZIP 时间与权限固定，PDF 无动态时间；生成与下载副本位于忽略的 `generated/`。完整 SHA-256 保存在上述自测 JSON，第二次生成可直接逐项比较。

| fixture_id | 名称/大小 | 内容判定 |
| --- | --- | --- |
| utf8-csv | `汇总 & # [验收] 100%.csv`，65 B | 3 行，金额 120.50 + 79.25 - 5.75 = 194；UTF-8 和 RFC 8187 名称保持 |
| workbook | `验收明细.xlsx`，2,421 B | 工作表 `明细`，3 行输入，数量合计 6、金额合计 194；由客户端计算输入列 |
| paragraph | `验收段落.pdf`，660 B | 1 页，Project Cedar、7 items、CEDAR-729 |
| same-name-a / same-name-b | 两份 `同名.csv`，各 17 B | 金额分别 11、29；按 fixture id 落盘，原名相同也保留两份 |
| legacy-over | 131,073 B | 超过现场旧描述 128 KiB；有效 CSV，金额 42 |
| inline-exact | 524,288 B | 源码默认解码边界；Base64 与完整双份 JSON 响应仍在 2 MiB 内 |
| inline-over | 524,289 B | inline 返回 LIMIT_EXCEEDED；GET 与资源模式可取完整字节，CSV 金额 42 |

HTTP 验收检查 octet-stream、attachment disposition、UTF-8 filename*、no-store、nosniff。负例覆盖无/坏 Bearer 401、其他有效主体 404、未知/过期句柄 404、过期重取 200、账户撤权 404、主体撤销 401、跨账户准备 NOT_FOUND、其他主体资源读取拒绝、超长 JSON-RPC id 413。报告只保存状态和合成内容摘要，不保存凭据、下载 URL 或句柄。

这些安全用例在探针内采用预置主体/ACL模拟；产品 issuer/audience/nbf/expiry/JWKS/refresh 等继续由 Rust OIDC 层及项目隔离 OAuth 验收负责。

## 官方接口核对与适配决定

资料于 2026-11-02 检索并打开核对：

1. [MCP tools / resource links](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#resource-links)：工具可返回资源 URI，客户端按资源协议读取；[资源协议](https://modelcontextprotocol.io/specification/2025-06-18/server/resources)的二进制内容是 Base64 blob。资源形状和宿主文件注册分属两层。
2. [MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http)：探针采用 JSON POST 响应、202 通知以及 GET 405 的最小组合，支持 2025-06-18/2025-11-25；未引入 SSE/会话。
3. [ChatGPT custom MCP server](https://developers.openai.com/api/docs/guides/custom-mcp-server)：目标客户端可连接远程服务并管理工具。此入口用于安排隔离客户端验收。
4. [OpenAI Plugins reference：File APIs](https://developers.openai.com/plugins/reference#file-apis)：uploadFile 提交用户选择的文件；selectFiles 选择宿主已授权文件；getFileDownloadUrl 为既有 fileId 取临时 URL；openai/fileParams 声明文件输入。任意邮件 Bearer URL 的服务端宿主注册路径仍待目标接口核验。
5. [ChatGPT UI](https://developers.openai.com/plugins/build/chatgpt-ui)：优先 MCP Apps 桥，宿主扩展按能力检测。UI 文件 API 可用性与后续分析能力一起纳入目标会话测试。

本轮选择一个清晰的适配边界：

| 事项 | 归属与约束 |
| --- | --- |
| 原始附件、引用、限额 | Rust `mail/attachments`；维持字节通道、MIME 索引、UIDVALIDITY、完整响应预算与全局槽位 |
| 文件落盘/注册与分析环境 | 客户端或连接器适配器；使用宿主正式接收接口，返回实际宿主文件引用；接收接口未通过时输出 client_delivery_required 与下一动作 |
| 令牌来源 | 标准客户端使用独立已授权会话；生产适配使用自有隔离 OAuth 客户端或宿主明确提供的委托接口。探针凭据由进程环境传入；自测凭据仅进程内生成 |
| 主体和账户 | 每个交付绑定 principal + AccountKey（账户 UUID/配置世代）+ MessageRef + AttachmentRef；GET/资源读取/注册前检查当前授权。现阶段产品单账户 default；探针模拟 default ACL |
| 注册权限 | 适配器只能提交当前已授权附件；文件内容完成后核验 SHA-256、长度和 MIME，再调用宿主接口；不读取 ChatGPT 连接器内部令牌 |
| 到期 | 邮件句柄保留 600 秒；过期用原稳定引用重新准备。适配器记录独立宿主文件有效期，注册后由宿主生命周期管理 |
| 撤销 | 下一请求重新检查主体/账户/世代；取消未完成交付，清理临时数据；已注册副本按宿主删除/撤销接口处理。R04需验收宿主副本撤销策略 |
| 隐私 | Bearer 只在 HTTPS Authorization header；不放工具输出、URL、模型上下文或日志。模型只接收附件描述、非保密交付引用和恢复动作 |

无认证签名链接代理未选为默认路径。若目标宿主最终仅支持 URL 拉取，R04 另做限时文件代理评审，固定单文件范围、一次性或短期授权、撤销、大小和并发额度，并明确 URL 作为访问凭证的处理方式。

## R04 可执行验收清单

1. 复用 `generate` 输出，在 Rust 隔离 IMAP fixture 与 HTTP handler 中验证 SHA-256、文件名、错误码、TTL、错误主体/账户、UIDVALIDITY、删除、坏编码、超限、慢客户端、取消和并发。
2. 创建或使用隔离 ChatGPT 连接，记录目标客户端版本/浏览器版本、MCP 协议版本、认证机制和工具清单；不改现有生产连接、不重新登录其 OAuth 客户端。
3. 针对 binary URL 与 resource_link 各运行一次，记录宿主是否自动取文件、是否携带已授权认证、返回什么文件引用。日志只记固定阶段和状态，不记认证头或下载 URL。
4. 接入宿主正式文件接收/注册接口或已授权委托组件，核验文件 digest；让目标分析环境计算 `验收明细.xlsx` 的 `明细` 数量/金额，以及两个同名 CSV 各自金额，提取 PDF 验证段落。
5. 使用 524,289 B CSV 验证自动大附件通路，再测试过期重新准备及撤销。保存客户端版本、脱敏工具结果形状、文件摘要、实际分析输出与错误恢复记录。

R00 的能力盘点、最小探针、选型和外部依赖已形成可审阅交付；R04 的自动文件注册与目标 ChatGPT 分析仍列为待验收项。
