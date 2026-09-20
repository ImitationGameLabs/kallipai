---
title: Agent core 与 shell
description: agent 运行时调优，以及注入 agent shell 会话的环境。
order: 30
---

[配置参考](../index.md)的一部分；本页覆盖 agent 运行时调优，以及注入 shell 会话的环境。

## Agent core

运行时调优变量见环境变量参考：
[Tagma](../../reference/environment-variables/tagma.md)。

### 变量间约束

部分变量带有启动时为隐式 profile 窗口强制执行的交叉校验：

- `KALLIP_OUTPUT_RESERVE_TOKENS` 必须严格小于活跃上下文窗口。
- `KALLIP_SUMMARY_MAX_TOKENS` 不得超过固定条目预算，即 `(上下文窗口 − KALLIP_OUTPUT_RESERVE_TOKENS) × KALLIP_PINNED_BUDGET_RATIO`。

这两项在启动时对隐式 profile 窗口（`KALLIP_CONTEXT_WINDOW_TOKENS`）检查；配置文件 profile 的窗口在 spawn 时逐 profile 检查（set 内故障转移时再惰性查一次）。配置文件 profile 窗口从不在 tagma 启动时校验。

- `KALLIP_CONTEXT_THRESHOLDS` 至少 2 个值，升序，各在 `1`–`99`。
- `KALLIP_TOKEN_BUDGET_WARNINGS` 至少 1 个值，升序，各在 `1`–`99`。

注入 shell 的变量集有独立页面：
[Agent shell 会话](../../reference/environment-variables/agent-shell.md)。

### 系统环境变量 {#system-environment-variables}

shell 后端从进程环境读取这些，传给每个被 spawn 的 `bash`：

| 变量     | 回退         | 用途                 |
| -------- | ------------ | -------------------- |
| `HOME`   | _（必需）_   | 用户家目录。         |
| `PATH`   | _（必需）_   | 系统 PATH。          |

后端还向每个被 spawn 的 `bash` 硬编码 `TERM=dumb`、`NO_COLOR=1`、`LS_COLORS=""`、`CLICOLOR="0"`，抑制彩色输出。
