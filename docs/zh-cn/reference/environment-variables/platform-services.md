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
| `KALLIPAI_MODEL_GATEWAY_ARCHEION_URL` | 否   | _（未设；管理面关闭）_  | 管理面鉴权所用的 archeion 内部根地址（如 `http://127.0.0.1:7100`）。须与 `KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE` 成对设置；缺任一，管理面拒绝一切请求，分发面与转发面不受影响。 |
| `KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE` | 否   | _（未设；管理面关闭）_  | archeion 内部共享令牌文件，启动时读一次；NixOS 模块固定指向 archeion 首启生成的 `/var/lib/kallipai/archeion/internal-token`，与 lesche、files、instances 读的是同一文件。 |
| `KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL` | 否   | `http://127.0.0.1:7501` | 网关对外发布的基址，作为每个分发 profile 的 `base_url`；消费者在其后追加 wire path（如 `chat/completions`）。`/v1` 后缀是客户端约定：网关自身的转发路由不带前缀，前置的 edge host 在代理前剥掉 `/v1`。直连数据面（前面没有剥前缀的 edge）时基址必须是不带路径的裸 origin：路径含 `/v1` 会得到 `404`。应使用 model-gateway host 形态，勿用 api-edge 的 `/v1/model-gateway` 段（该段路由到管理面，管理面没有转发路由）。 |
