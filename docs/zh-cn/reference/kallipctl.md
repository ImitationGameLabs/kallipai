---
title: kallipctl CLI 参考手册
description: 面向运维的实例管理工具 CLI 参考。
order: 75
---

`kallipctl` 通过守护进程的控制套接字管理本地 kallip 实例：spawn、adopt、
start/stop/restart、持久化 env、日志与 blob 存储维护。守护进程部署见
[部署文档](../deployment/nixos/minimal.md)；套接字的 `0600` 权限即认证。

## Shell 补全

`kallipctl` 内置隐藏的 `generate` 动词，按 shell 输出补全脚本：

```bash
kallipctl generate bash    # 可选 zsh、fish、elvish、powershell
```

nix 安装（`kallipctl`、`kallipai-admin` 或 `workspace` 包）在构建期生成并
自动安装：bash、zsh、fish 补全落入各 shell 的标准 profile 路径；五种
shell 的脚本同时保留在 `share/kallipai/completions/kallipctl/` 下，便于
手工安装到其它环境。

非 nix 安装可在 shell 启动脚本中接入（bash 示例）：

```bash
kallipctl generate bash > ~/.local/share/bash-completion/completions/kallipctl
```

## 动态补全（可选）

安装的脚本覆盖子命令与旗标补全。若需值级实时补全（实例 slug，随输入向
守护进程查询），改用动态钩子。守护进程不可达时自动退化为空候选，不会
阻塞输入：

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```
