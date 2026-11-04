---
order: 40
title: 服务模型与运维
description: 各服务如何以 systemd unit 运行，以及令牌轮换等日常运维。
---

本章讲部署底下的服务模型：内部令牌、启动身份，以及启动时值得识别的故障形态。

## 内部令牌（自管理）

polis 服务之间用一枚共享密钥相互认证，由控制面供给到其状态目录（`/var/lib/kallipai/archeion/internal-token`，权限 0640，模块创建的 `kallipai-polis` 组可读）。其余五个服务读同一文件，六者在部署生命周期内共享同一个值。

密钥状态按生命周期归档：`/etc` 存管理员所有的静态配置（钉死的管理令牌），`/run` 存易失的运行时状态，`/var/lib` 存服务所有的持久状态（内部令牌）。

轮换内部令牌：停掉六个 polis 服务，删除该文件，启动 archeion（会生成新值），再启动其余五个。每个读者服务都在启动时只读一次令牌文件，留在运行中的服务会带着旧值继续调 `/internal`，因此必须随整组重启。整组重启保证所有服务处于同一 generation。

### 用户、启动身份与 real-root 防护

模块的 daemon unit 以 `root` 运行（系统服务，需要读取每个声明用户的 passwd 条目），而声明的 `tagmaUsers` 是实例唯一的启动身份。root 身份的 daemon 拒绝推断式就地启动（报错会要求 `--user`），因此 NixOS 上的每次 `adopt` 与 `start` 都要点名一个声明用户；平台承载的任何东西都不会以主机真实 root 运行，直接以真实 root 启动的 tagma 会拒绝引导（逃生口是 `KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT=1`，不适用于本部署形态）。

声明的用户随模块供给家目录与 linger：`/home/<user>` 存放实例的 XDG config、data 与 state 树，logind 在开机时预建 `/run/user/<uid>`，即被拉起实例的 `XDG_RUNTIME_DIR`，无需逐实例设置。服务所有的持久状态（polis 服务的存储与内部令牌）留在 `/var/lib/kallipai`，与逐用户家目录分开：即内部令牌一节描述的同一套生命周期划分。

## 查看观测栈

平台主机默认运行观测栈：Grafana 在 `127.0.0.1:3000`，Prometheus 在
`127.0.0.1:9090`，tempo 在 `127.0.0.1:3200`，OTLP 接收端在
`127.0.0.1:4318`（HTTP）与 `127.0.0.1:4317`（gRPC）。它们全部只绑回环，
另一台机器上的浏览器经由隧道访问：

    ssh -N -L 3000:127.0.0.1:3000 -L 9090:127.0.0.1:9090 admin@host

Grafana 以匿名 Viewer 打开，Prometheus 与 tempo 两个数据源已预置，trace id
可直接粘进 Explore。管理员登录名是 `admin`，密码在
`/var/lib/grafana/admin-password`；在主机上用
`grafana cli admin reset-admin-password` 重置。

四个单元是 `prometheus`、`grafana`、`prometheus-node-exporter` 与
`kallipai-tempo`，`systemctl status` 可列出它们，
`journalctl -u kallipai-tempo` 显示 trace 存储如何读它的配置。把
`services.kallipai.observability.enable` 设为 `false` 会停掉这四个单元，
被观测的服务不受影响。

## 首次启动排障

内部令牌文件缺失在 archeion 首次启动时不算错误；它会生成一枚。lesche、keryx、files、instances 与 model-gateway unit 依赖 archeion 并在启动时读取该文件；短宽限窗内文件未出现，unit 就拒绝启动，`journalctl -u kallipai-instances` 会显示它等待的路径。空令牌文件会让所有读取方显式失败；删掉文件重新供给，不要手工编辑。
