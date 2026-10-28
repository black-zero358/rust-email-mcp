# 2026-10-29 未知 transfer encoding 处理（issue #34）

- 日期/环境：2026-10-29，Asia/Shanghai；Windows 独立工作树、WSL Ubuntu，固定 Rust/Cargo 1.99.1。独立 target `/home/oeic_fe/issue-cte-target`；复制依赖缓存后清除本项目及 vendored async-imap 构建产物并重编，明确执行新增测试 1 项。
- 修复代码：`08ff18477f2b32f993e26681daff014d89caedde`，基线 main `18b26ddc8a365ecdabb7a38b50a476b8f9155679`；整合提交 `949686f1e28fb9d1ec8fdd5080f2b27705be742f` 合入 main `717a302`，无冲突、无额外运行时手改。

## 根因和行为

mailparse 0.18.0 的 `Body::new` 对未知 CTE 默认为 SevenBit。修复前新增 `transfer_encodings_require_known_decoders_before_interpretation` 失败：预期 `UNSUPPORTED_BODY`，实际为成功字符串。修复后编码支持性先于实体解释与叶子解码检查，只有缺省及五种受支持编码可进入 codec。非空未知编码内联实体明确拒绝，alternative 可回滚并选完整受支持候选；未知编码 multipart 不遍历原始字节中的伪边界。

`body_raw` 同样拒绝未知编码，避免把编码原始数据当作已解码附件；不新增原始编码下载契约或更改工具字段。显式附件仍可发现，普通读取与转发排除它；内联下载返回既有 `UNSUPPORTED_BODY`，认证二进制 GET 保留 HTTP 502 后端失败映射。真正空实体继续可以表示空正文，显式空/空白编码的非空正文不当作缺省 7bit。

## 已执行验证

- 整合提交使用独立 target 运行 `cargo fmt --check`、`cargo test --locked`：85 单元 + 1 IMAP 集成测试通过；`cargo clippy --locked --all-targets -- -D warnings` 和 `cargo build --locked --release` 通过。
- `transfer_encodings_require_known_decoders_before_interpretation` 覆盖缺省、7bit、8bit、binary、Base64、quoted-printable、大小写、未知编码、不透明 multipart、下载拒绝及 mixed 候选回滚。
- `unknown_transfer_encoding_cannot_be_read_forwarded_or_downloaded_as_decoded` 同时验证读取和转发拒绝、plain/HTML/multipart、未知与空编码、mixed 不能只保留可见片段、alternative 完整回退，以及附件列表索引和下载的明确失败。
- 专用 `bash scripts/audit-dependencies.sh`（cargo-audit 0.22.2）覆盖 307 项依赖并通过；依赖、锁文件与 vendored 补丁不变。`git diff --check` 通过。保留 vendored 既有 deprecated 宏警告。
- 最终 PR 的 `check`、`audit`、`container` 须全部成功后再合并；后续整合验证通过 PR COMMENT 绑定新 head，不用历史结果替代。

## 兼容性与边界

修正未知编码被当作成功解码的行为；旧工具参数、字段、权限、配置和操作库格式不变，无迁移。标准叶子编码和预算维持；未知数据不再被错误地交付为原 Content-Type 下的正文或已解码附件。

仅合成数据与本地模拟后端，没有真实邮箱读取/写入、生产部署、压力实验或 musl 产物验收。默认 Linux GNU release 构建不能替代 musl 交付验证。

依据：[RFC 2045 §6.1](https://www.rfc-editor.org/rfc/rfc2045#section-6.1)、[§6.4](https://www.rfc-editor.org/rfc/rfc2045#section-6.4)，以及本地锁定 mailparse 0.18.0 的 `body.rs`。Context7 未索引 Rust mailparse，已核对官方 docs.rs 与实际依赖源码。
