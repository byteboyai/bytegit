# bytegit

ByteBoy 系产品(Dozer、Digger……)共用的**本地 Git 底层库**。

- 全部同步 API,不依赖 tokio / iced。
- 公开类型不暴露 `git2`,调用方不被 `git2` 版本锁死。
- 不含托管平台账户、提交图布局、worktree/commit/merge(见设计规格)。

## 依赖

```toml
bytegit = { git = "https://github.com/byteboyai/bytegit", tag = "v0.1.0" }
# 下游测试里要用确定性临时仓库夹具:
# bytegit = { git = "...", tag = "v0.1.0", features = ["testutil"] }
```

一律用 tag。本地联调在使用方的 `.cargo/config.toml` 里(不提交):

```toml
[patch."https://github.com/byteboyai/bytegit"]
bytegit = { path = "../bytegit" }
```

## 设计

dozer 仓库 `docs/superpowers/specs/2026-10-02-bytegit-design.md`。
