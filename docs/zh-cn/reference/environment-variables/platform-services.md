---
title: 平台服务
description: archeion、lesche、files、instances 与 model gateway 服务进程的可选滚动文件日志。
order: 30
---

平台服务进程（archeion、lesche、files、instances、model gateway）的可选滚动文件日志。每个变量让该服务按日轮转的文件成为唯一事件通道；stdout 只打一行路标后不再输出。

| 变量                      | 必填 | 默认                   | 说明                                                                                                                                                                                |
| ------------------------- | ---- | ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIPAI_ARCHEION_LOG_DIR` | 否   | _（未设；仅 stdout）_  | archeion 的可选滚动文件日志。设为目录：按日轮转的文件（保留七个，`archeion.<date>.log`）成为唯一事件通道，stdout 只打一行路标（服务名、当前日志文件、级别阈值）后不再输出。目录建不出或写入器构建失败则以非零码退出并打一行 stderr。未设或空保持仅 stdout 行为。目录属主与权限是部署事务（systemd `LogsDirectory`）。 |
| `KALLIPAI_LESCHE_LOG_DIR`   | 否   | _（未设；仅 stdout）_  | lesche 的可选滚动文件日志，语义与 `KALLIPAI_ARCHEION_LOG_DIR` 相同（文件为唯一事件通道、按日轮转、保留七个文件、`lesche.<date>.log`）。       |
| `KALLIPAI_FILES_LOG_DIR`    | 否   | _（未设；仅 stdout）_  | files 服务的可选滚动文件日志，语义与 `KALLIPAI_ARCHEION_LOG_DIR` 相同（文件为唯一事件通道、按日轮转、保留七个文件、`files.<date>.log`）。      |
| `KALLIPAI_INSTANCES_LOG_DIR`    | 否   | _（未设；仅 stdout）_  | instances 代理的可选滚动文件日志，语义与 `KALLIPAI_ARCHEION_LOG_DIR` 相同（文件为唯一事件通道、按日轮转、保留七个文件、`instances.<date>.log`）。 |
| `KALLIPAI_MODEL_GATEWAY_LOG_DIR` | 否   | _（未设；仅 stdout）_  | model gateway 的可选滚动文件日志，语义与 `KALLIPAI_ARCHEION_LOG_DIR` 相同（文件为唯一事件通道、按日轮转、保留七个文件、`model-gateway.<date>.log`）。 |
