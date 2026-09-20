---
title: Tagma 服务
description: 配置 tagma 服务端的环境变量。
order: 40
---

[配置参考](../index.md)的一部分；本页覆盖服务端进程本身：监听地址、数据与 skills 根、日志。

tagma 服务器变量：
[Tagma](../../reference/environment-variables/tagma.md)。

## `ADVERTISE_URL` 与 `TAGMA_URL` 之别 {#advertise_url-vs-tagma_url}

两者用途相关而不同：

- **`KALLIP_ADVERTISE_URL`**：由 operator 配置。告诉 tagma“别人该用这个 URL 访问你”。tagma 把该值注入子进程。
- **`KALLIP_TAGMA_URL`**：由客户端（CLI）消费。告诉它们“tagma 在哪”。tagma 启动时自动从 `ADVERTISE_URL` 设置。

常见情形（一切都在 localhost）两者同值。容器或反向代理设置里，内部监听地址与外部可达 URL 不同，两者随之分叉。

### 数据与 skills {#data-and-skills}

| 变量                 | 必填 | 默认                          | 说明                                                                                                                                                                                                                                                                                                                                                                        |
| -------------------- | ---- | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| （数据根）           | 否   | _（推导）_                    | 从 `KALLIP_TAGMA_SLUG` 在平台数据目录下推导（见[本地 daemon](../../reference/environment-variables/daemon.md)）；没有可设的逐实例数据目录变量。运行时在其下直接写 `agents/`（含 `active/`、`inactive/`、`archived/` 生命阶段目录）与 `skills/`。                                                                                                                            |
| `KALLIP_SKILLS_ROOT` | 否   | 实例数据根的 `skills/`        | 共享 skill 目录的直接路径。按原样使用（不追加后缀）。                                                                                                                                                                                                                                                                        |
| `KALLIP_SKILLS_SEED` | 否   | _（未设；nix wrapper）_       | 预置 skill 默认的只读树（一个 nix store 路径）。tagma 首次启动且共享 skill 目录为空时，其内容被复制进去。目标是 `KALLIP_SKILLS_ROOT`（若设），否则实例数据根的 `skills/`；`KALLIP_SKILLS_ROOT` 只改目标位置，不禁用预置。目标已非空时跳过（绝不覆盖）。nix 安装下 workspace 构建经 `kallip-tagma` wrapper 已带此默认（`--set-default`：仅在变量未设时生效，显式环境值或容器镜像 Env 仍胜出）。显式空值禁用预置：wrapper 原样保留，tagma 将其滤除。 |

### 日志 {#logging}

| 变量                      | 必填 | 默认                   | 说明                                                                                                                                                                                |
| ------------------------- | ---- | ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`                | 否   | `info`                 | 标准 env-filter 语法。控制 tagma 日志详细度。示例：`kallip_client=debug`。                           |

平台服务日志目录：
[平台服务](../../reference/environment-variables/platform-services.md)。
