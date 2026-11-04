---
title: 基础部署
description: 用一份配置文件在单台 NixOS 主机上启动整个平台。
order: 10
---

## 前置条件

- 一台启用 flakes 的 NixOS 主机（`nix.settings.experimental-features = [ "nix-command" "flakes" ]`）。

## 导入模块

新增 kallipai 这个 flake input，再在 NixOS 配置的 modules 列表里加入 `inputs.kallipai.nixosModules.kallipai`：

```nix
{
  inputs.kallipai.url = "github:ImitationGameLabs/kallipai";

  outputs = { nixpkgs, ... }@inputs: {
    nixosConfigurations."<myhost>" = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        inputs.kallipai.nixosModules.kallipai
        ./configuration.nix
      ];
    };
  };
}
```

## 基础配置

```nix
{ config, ... }:
let
  domain = "kallipai.lan";
  ports = config.services.kallipai.polis.ports;
in
{
  services.kallipai = {
    inherit domain;
    tls = false;
    daemon.enable = true;
    polis.enable = true;
    web.enable = true;
  };

  services.caddy = {
    enable = true;
    virtualHosts = {
      # The web app: the SPA with the runtime config baked in.
      "http://app.${domain}".extraConfig = ''
        root * ${config.services.kallipai.web.distWithRuntimeConfig}
        try_files {path} /index.html
        file_server
      '';

      # The platform's single API face: path-routed per service.
      "http://api.${domain}".extraConfig = ''
        # The archeion's internal surface stays off the public face.
        @internal path /v1/archeion/internal /v1/archeion/internal/*
        handle @internal {
          respond 404
        }
        handle_path /v1/archeion/* {
          reverse_proxy 127.0.0.1:${toString ports.archeion}
        }
        handle_path /v1/lesche/* {
          reverse_proxy 127.0.0.1:${toString ports.lesche} {
            # lesche serves SSE streams; flush_interval -1 disables
            # response buffering so events reach clients immediately.
            flush_interval -1
          }
        }
        handle_path /v1/keryx/* {
          reverse_proxy 127.0.0.1:${toString ports.keryx} {
            # keryx streams like the lesche; flush_interval -1 disables
            # response buffering so events reach clients immediately.
            flush_interval -1
          }
        }
        handle_path /v1/instances/* {
          reverse_proxy 127.0.0.1:${toString ports.instances}
        }
        @files path /v1/files /v1/files/*
        handle @files {
          uri strip_prefix /v1/files
          reverse_proxy 127.0.0.1:${toString ports.files}
        }
        # The model gateway's own API as the sixth /v1 service segment.
        # Everything under the segment is the gateway's management face
        # (profile reads, the admin family, health, metrics), so the
        # stripped path routes on the management port. Every route on
        # that face carries its own family credential (an admin token or
        # a proxy key) except the unauthenticated health and metrics
        # probes; the edge adds none.
        handle_path /v1/model-gateway/* {
          reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.ports.model-gateway}
        }
      '';

      # The model gateway: its own host for the OpenAI wire (clients
      # configure a /v1 base; the edge strips it before the gateway).
      "http://model-gw.${domain}".extraConfig = ''
        handle_path /v1/* {
          # Streaming wire (chat completions stream): like the lesche
          # segment, disable buffering so tokens reach clients live.
          reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.model-gateway.forwardPort} {
            flush_interval -1
          }
        }
        # The fallback carries the same streaming surface.
        reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.model-gateway.forwardPort} {
          flush_interval -1
        }
      '';
    };
  };

  networking = {
    firewall = {
      allowedTCPPorts = [
        80
      ];
    };

    hosts = {
      "127.0.0.1" = [
        "app.kallipai.lan"
        "api.kallipai.lan"
        "model-gw.kallipai.lan"
      ];
    };
  };
}
```

## rootless 使用

要用 `kallipctl` 驱动 daemon，把用户加入 daemon 的 socket 组：控制 socket 位于 `/run/kallipai/daemon.sock`，仅对 `kallipai` 组开放（把 `<username>` 换成你的用户名）：

```nix
users.users."<username>".extraGroups = [ "kallipai" ];
```

## 部署与验证

在 NixOS Configuration 成功构建并切换到新的 Generation 之后，我们通过下面方式进行验证：

检查七个 unit：

```sh
systemctl status kallipai-daemon kallipai-archeion kallipai-lesche \
  kallipai-keryx kallipai-files kallipai-instances kallipai-model-gateway
```

写日志文件的服务会在 `/var/log/kallipai/<service>/` 下保留按天滚动的 `<service>.<date>.log`（保留七个）。该目录权限为 `0750`，属主是服务用户，组是 `services.kallipai.daemon.group`（默认 `kallipai-polis`）：以该组成员身份读取，或使用 `sudo`。

然后确认三个域名都有响应：`api.kallipai.lan` 承载平台，`app.kallipai.lan` 承载 web 应用，`model-gw.kallipai.lan` 承载模型网关。部署健康的标志：应用能加载，能注册登录，能创建第一个 agent。网关提供健康端点：

```sh
curl http://model-gw.kallipai.lan/health
```

`/metrics` 指标面在管理端口上，不在 model-gw 域名上：在主机本机访问
`http://127.0.0.1:7500/metrics`，或经 api 边缘访问
`http://api.kallipai.lan/v1/model-gateway/metrics`。它是转发可观测性
计数器的 Prometheus 文本，无需凭据：探针必须在凭据故障时仍可抓取。

NixOS 模块自行启动主机的可观测性栈：Prometheus、Grafana、node
exporter 与 tempo trace 存储，全部只绑回环，外加平台服务导出的
OTLP/HTTP 接收端。`services.kallipai.observability.enable` 默认跟随
polis 开关，因此平台主机获得这套栈、仅 daemon 的主机不获得；在自行运行
记录器的 polis 主机上设为 `false`。

端口是选项，引用端口的地方全部跟随：`services.kallipai.observability.ports`
包含 `prometheus`（9090）、`grafana`（3000）、`tempo`（3200）、
`otlp`（4318）、`otlp-grpc`（4317）与 `node-exporter`（9100）。保留期
为指标 30 天（`retention.metrics`）、trace 72 小时（`retention.traces`），
与 compose 形态的清理预算相同。存储在 `/var/lib/prometheus2`、
`/var/lib/grafana` 与 `/var/lib/kallipai/telemetry/tempo`。

模块向记录器接入三个任务：平台自身的指标面、node exporter，以及 tempo
自己的 `/metrics`，都无需额外接线；Grafana 预置 Prometheus 与 Tempo 两个
数据源。只读列表
（`config.services.kallipai.polis.scrapeConfigs`）保留给自建 Prometheus
的主机；那种主机要把栈关掉，否则模块自己会追加同一份列表，Prometheus 因同名
任务拒绝启动：

```nix
services.kallipai.observability.enable = false;
services.prometheus = {
  enable = true;
  scrapeConfigs = config.services.kallipai.polis.scrapeConfigs;
};
```

看板以 Viewer 身份匿名开放；管理员密码首次启动时以 0600 生成在
`/var/lib/grafana/admin-password`，从不进入 store。在主机上用
`grafana cli admin reset-admin-password` 重置。追踪端点送达每个平台服务
以及 daemon 启动的每个实例；环境里带通用键 `OTEL_EXPORTER_OTLP_ENDPOINT`
的实例上报 `service.instance.id=<slug>`，同一构建的多个实例在看板里互不
混淆。端点只在实例启动时写进它自己的环境。
`nixos-rebuild switch` 会重启平台单元（含 daemon），但实例不是 systemd 单元：
被接管的实例跨过 switch 用启动时的环境继续跑，各自重启后才开始上报。既有的
polis 主机升级后即获得这些单元；上面的开关可关闭它们。compose 形态在容器里运行同一套存储，指南见
`docs/en/deployment/container.md`。

## 网关管理面

模型网关的管理面在同一端口上承载两族凭据：代理键授权分发读与转发面，
平台身份授权 /admin 路由族。两族不共享路径，也不共享失败路径，因此
一支凭据推不出另一支。

/admin 路由族的凭据即平台身份（archeion）。请求要么携带 archeion
admin bearer（机器与 CLI 通道），要么携带本地管理员的平台会话
cookie（浏览器通道：管理页以平台账户登录）。有效的非管理员身份返回
403（已认证但未授权）；凭据无效或缺失返回 401；archeion 不可达、应答
调用契约外的状态码、或应答体无法解码时，一律返回 503，按失败关闭处理；
应答自相矛盾（自称用户或 tagma 却没有凭据记录）时返回 500，同样失败关闭，
重试解不了，只有升级能解。未配置
archeion 接线时整个管理面关闭，分发面与转发面照常运行。

浏览器通道成对加固：会话 cookie 为 HttpOnly 与 SameSite=Strict，且
状态变更请求必须携带跨源浏览器无法伪造的自定义头。会话在服务侧仅存
opaque 哈希。归因以凭据为准：每次管理变更都把操作者写入审计轨迹，
profile 与 set 记录归属账户。

## 管理令牌

`adminTokenFile` 未设置时，archeion 首次启动在状态目录铸造管理令牌
（`/var/lib/kallipai/archeion/admin-token.env`，权限 0600），此后只读取、
不再改写。没有任何命令打印现值：服务端从不返回它，轮换（回的是新值）是
唯一取到的途径：

```sh
sudo kallipai-admin admin-token reset
```

reset 用当前令牌认证，令牌取自 `KALLIPAI_ARCHEION_ADMIN_TOKEN`，变量未设
时直接读上面那个状态文件；调用服务端前它还会本地读一次该文件，供预检。
因此可在 archeion 主机上直接运行，无需先导出（已导出时以导出值为准，`sudo -E` 可保留它）。新值只打印这一次，之后无处可取，务必当场保存。

对接平台的日常操作应给 `kallipctl` 用个人访问令牌，而不是管理令牌：
在 Web 界面按需勾选字符串创建，写入
`$XDG_CONFIG_HOME/kallipai/access-token`（权限 0600），再用
`kallipctl spawn <slug> <workspace> --polis <origin>`。完整流程见 kallipctl 参考手册。

设置 `adminTokenFile` 时使用操作者钉住的令牌，轮换会被拒绝。

从铸造切换到钉住：设置 `adminTokenFile` 并重新部署后，archeion 改用钉住的值；状态文件仍留在盘上，不再被铸造，但环境变量未设时 `kallipai-admin` 仍会回落到它。建议删除该文件，免得留着一枚过期令牌被当作认证值。
