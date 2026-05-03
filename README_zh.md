# Warp 中文社区版

本仓库是 [warpdev/warp](https://github.com/warpdev/warp) 的**中文社区 fork**：客户端 UI 全量汉化，默认中文，可在设置中切换为英文或跟随系统。

> 本 fork 仅汉化客户端 UI（菜单、命令面板、设置、对话框、Tooltip、Block actions、Resource Center、Onboarding 等）；终端命令输出、服务端 GraphQL 字段、AI 模型回复保持原样不变。

## 语言切换

打开 **设置 → 通用 → 语言**：

- `中文（简体）` — 强制中文（fork 默认）
- `English` — 强制英文
- `跟随系统` — 系统 locale 为 `zh*` 时使用中文，否则英文

切换不需要重启 Warp。设置项位于 `~/.warp/settings.toml` 的 `language` 字段。

## 自动更新（无需 Apple Developer ID）

本 fork 通过 GitHub Releases 实现客户端自更新，**不依赖任何后端服务器**。机制：

- CI 在打 `v*` tag 时产出 `.tar.gz` + `.minisig` 资产，使用 minisign 私钥签名（私钥仅存在于 GitHub Actions Secret）。
- 客户端启动后到 **设置 → 账户 → 版本**，点击 **下载并安装**，进程内通过 `reqwest` 拉取并验签，`tar` 解压后原地替换、重启。
- 进程内下载不会附加 `com.apple.quarantine`，因此 ad-hoc 签名的 `.app` 替换后 Gatekeeper 不会重新评估，更新对用户静默。

**首次安装** 是唯一需要手动操作的一次（macOS 不允许应用绕过 Gatekeeper 自我授权）：

```bash
xattr -dr com.apple.quarantine /path/to/Warp-cn.app
```

或在 系统设置 → 隐私与安全性 中点击「仍要打开」。**之后所有自动更新永久静默生效。**

> 维护者 fork 后首次启用更新通道：执行 `script/generate_update_keys.sh` 生成 minisign 密钥对，将公钥提交到仓库（`script/warp-update.pub`），私钥放入 GitHub Actions Secret `MINISIGN_SECRET_KEY`。

## 与上游同步

本 fork 维护者会定期 merge upstream。每个含 UI 字符串的 PR 拆为两步：

1. **结构步**：`t!("...")` 替换 inline 字面量 + 同步 `bundles/en/*.ftl`
2. **翻译步**：填充 `bundles/zh-CN/*.ftl`

详见 `crates/warp_i18n/MERGE_NOTES.md`。

## 贡献翻译 / 修订

- 新增字符串 / 修订术语：见 `docs/i18n.md`
- 术语锁定：`crates/warp_i18n/GLOSSARY.md`
- Lint 与 CI：`cargo xtask check-i18n --mode hard`
- Bundle 对齐校验：`cargo xtask check-i18n --check-parity`

## 与上游差异

- 客户端 UI ~7000+ 字符串汉化（不影响功能 / 性能）
- 新增 crate：`warp_i18n`、`warp_i18n_macros`
- 新增 CI workflow：`.github/workflows/i18n-lint.yml`（`hard` 模式 + allowlist 冻结）
- `crates/warp_server_client` 增加 `RemoteString` marker，标记不可本地化的服务端字段

## 上游 README

通用项目说明、构建、贡献流程、许可证：见 [`README.md`](./README.md)。
