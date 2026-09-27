---
title: 在 Linux 上安装（FHS）
description: 将 KallipAI tarball 安装到常规 x86_64 Linux 发行版：用户安装、systemd 配置、升级与卸载。
order: 15
---

本指南覆盖 FHS 安装形态：一个自包含的 tarball，可运行在任何具备 glibc 2.35 及以上的 x86_64 Linux 发行版上。NixOS 主机请优先使用 [NixOS 模块](nixos/index.md)；FHS 形态面向 Ubuntu 22.04+、Debian 12+、Fedora 36+、Arch 与 RHEL 10+。
v1 FHS 形态覆盖二进制与 systemd 单元；polis 侦听的反代与 TLS 终结、以及 web 应用不在其范围内。

## 前置条件

- 一台 x86_64 Linux 机器，glibc 2.35 及以上。安装脚本对两者都会检查，不匹配时明确报错拒绝。
- `curl`、`tar`、`sha256sum`（受支持的发行版默认都具备）。
- systemd 安装形态：root 权限与正在运行的 systemd。

## 快速开始

默认安装是按用户的：无需 root，不装 systemd 单元。

下载模式必须指明版本：传 --version（发布就绪后任何已发布版本均可）。

```bash
curl -fsSL https://raw.githubusercontent.com/kallipai/kallipai/main/install.sh | bash -s -- --version 1.0.0
```

运行前先读一遍脚本是个好习惯，同一份安装的审阅优先形态：

```bash
curl -fsSL https://raw.githubusercontent.com/kallipai/kallipai/main/install.sh -o install.sh
less install.sh
bash install.sh
```

也可以从本地 tarball 安装而不下载（发布 tarball 附带 `.sha256` 伴生文件，安装脚本会在解包前校验）：

```bash
bash install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz
```

## 各内容的位置

版本化安装根与数据根是分离的，安装脚本只写前者：

- 载荷：`~/.local/lib/kallipai/<版本>/`，每个版本一个目录。
- 链接：`~/.local/bin/<名称>`，指向载荷。若该目录不在你的 `PATH` 上，安装脚本会打印提示行（内容为把该目录加入 `PATH` 的导出语句，按 shell 给出）。
- 数据：`~/.local/share/kallipai`，由工具在运行时创建，安装脚本从不触碰。

## 升级

安装是幂等的。用更新的 tarball 重跑同一脚本，链接即改指新版本；旧版本目录随后被清理，除非仍有进程在从旧目录运行（安装脚本会报告这些进程，而不会杀死任何进程）。

## 卸载

```bash
bash install.sh --uninstall
```

卸载会删除载荷树与指向它的链接（指向其他目标的链接保持不动）。你在 `~/.local/share/kallipai` 的数据会保留。
系统级安装的卸载遵循同一模式：`sudo bash install.sh --with-systemd --uninstall`。root 会删除载荷、链接与五个单元文件，保留 `/var/lib/kallipai`（服务状态树）与系统用户、组，并打印手动清理提示。

## systemd 系统级安装

要获得带服务的主机级安装，加 `--with-systemd`。它需要 root 与仓库检出（单元模板位于 `nix/install/systemd/` 下）：

```bash
sudo bash install.sh --tarball ./kallipai-1.0.0-linux-x86_64.tar.gz --with-systemd
```

这会把载荷装到 `/usr/local/lib/kallipai/<版本>/`、链接到 `/usr/local/bin`、创建专用系统用户，并向 `/etc/systemd/system/` 渲染五个单元：

- `kallipai-daemon`（以 root 运行；`KillMode=process`，守护进程重启时运行中的 tagma 不受牵连）
- `kallipai-archeion`、`kallipai-lesche`、`kallipai-files`、`kallipai-instances`（各用专用用户）

单元会被 enable，升级时已处于 active 的单元会被重启。用更新的 tarball 重跑同一命令即完成原位升级。

polis 服务预期存在 PostgreSQL，每个服务一个数据库与角色（本地 unix socket 上的 peer 认证），与 NixOS 模块默认一致；数据库本身由部署者提供。可选项放在 drop-in（`systemctl edit <单元>`）里；每个单元模板的头注释都带一个示例。
例如在原生 Debian 系主机上：

```bash
sudo -u postgres createuser kallipai-archeion
sudo -u postgres createdb -O kallipai-archeion kallipai-archeion
```

对 `kallipai-lesche` 与 `kallipai-files` 重复这一对命令；peer 认证走本地 unix socket。

## 无 systemd 的环境

在没有运行 systemd 的主机上，`--with-systemd` 仍会安装载荷与链接并渲染单元，但打印提示而不 enable。可直接运行二进制，或用你自己的 init 系统管理。
