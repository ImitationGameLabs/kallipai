---
title: 平台服务
description: archeion、lesche 与 files 服务进程的可选滚动文件日志。
order: 30
---

三个平台服务进程（archeion、lesche、files）的可选滚动文件日志。每个变量把该服务的日志输出导入按日轮转的文件，stdout 继续流出。

| 变量                      | 必填 | 默认                   | 说明                                                                                                                                                                                |
| ------------------------- | ---- | ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_ARCHEION_LOG_DIR` | 否   | _（未设；仅 stdout）_  | archeion 的可选滚动文件日志。设为目录：事件双写按日轮转的文件（保留七个，前缀 `archeion.log.`），stdout 继续流出供 journald 采集。未设或空保持历史仅 stdout 行为；目录建不出则降级为仅 stdout 并记启动通知。目录属主与权限是部署事务（systemd `LogsDirectory`）。 |
| `KALLIP_LESCHE_LOG_DIR`   | 否   | _（未设；仅 stdout）_  | lesche 的可选滚动文件日志，语义与 `KALLIP_ARCHEION_LOG_DIR` 相同（双写、按日轮转、保留七个文件、前缀 `lesche.log.`）。       |
| `KALLIP_FILES_LOG_DIR`    | 否   | _（未设；仅 stdout）_  | files 服务的可选滚动文件日志，语义与 `KALLIP_ARCHEION_LOG_DIR` 相同（双写、按日轮转、保留七个文件、前缀 `files.log.`）。      |
