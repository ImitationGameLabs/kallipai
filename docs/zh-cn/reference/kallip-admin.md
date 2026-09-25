---
title: kallip-admin CLI 参考手册
description: 无头 archeion 管理工具的 CLI 参考。
order: 76
---

`kallip-admin` 是面向 archeion 中继的无头运维 CLI：通过 HTTP 驱动
`/admin/*` 面（用户、passkey、注册码、admin 令牌生命周期），以
`KALLIP_ARCHEION_ADMIN_TOKEN` 环境变量认证。令牌刻意只走环境变量：
命令行旗标会把密钥泄漏进 `ps`、`/proc/<pid>/cmdline` 与 shell 历史。

## Shell 补全

`kallip-admin` 内置隐藏的 `generate` 动词，按 shell 输出补全脚本：

```bash
kallip-admin generate zsh    # 可选 bash、fish、elvish、powershell
```

nix 安装（`kallip-admin`、`kallipctl` 或 `workspace` 包）在构建期生成并
自动安装：bash、zsh、fish 补全落入各 shell 的标准 profile 路径；五种
shell 的脚本同时保留在 `share/kallipai/completions/kallip-admin/` 下，
便于手工安装到其它环境。

非 nix 安装可在 shell 启动脚本中接入（zsh 示例）：

```bash
mkdir -p ~/.zfunc && kallip-admin generate zsh > ~/.zfunc/_kallip-admin
# 在 ~/.zshrc 的 compinit 之前：
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```
