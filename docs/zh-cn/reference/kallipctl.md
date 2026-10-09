---
title: kallipctl CLI 参考手册
description: 面向运维的实例管理工具 CLI 参考。
order: 75
---

`kallipctl` 通过守护进程的控制套接字管理本地 kallip 实例：spawn、adopt、
start/stop/restart、持久化 env、日志与 blob 存储维护。守护进程部署见
[部署文档](../deployment/nixos/minimal.md)；套接字的 `0600` 权限即认证。
polis 服务自身的日志可用 `logs --service <name>` 经守护进程追踪本机
`/var/log/kallipai/<service>` 下的滚动文件（守护进程代读，无需 sudo）；
裸 `kallipctl logs` 列出本机有日志的服务。

## Shell 补全

补全是动态的：钩子随输入实时向守护进程查询（实例 slug、服务名）；
守护进程不可达时自动退化为空候选，不会阻塞输入：

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```

nix 安装（`kallipctl` 或 `workspace` 包）在构建期生成这些钩子脚本
并自动安装：bash、zsh、fish 落入各 shell 的标准 profile
路径，五种 shell 的脚本同时保留在 `share/kallipai/completions/kallipctl/`
下。每份钩子脚本内嵌其二进制的 nix store 路径，拷贝到其它主机会静默
失效，应在目标主机重新生成。

隐藏的 `generate` 动词输出静态（AOT）脚本：词表在构建期烧定、不查询
守护进程。nix 安装不为 kallipctl 使用它们；适合补全时无法运行 CLI
的封闭环境：

```bash
kallipctl generate bash    # 可选 zsh、fish、elvish、powershell
```
