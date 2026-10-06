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
        handle_path /v1/instances/* {
          reverse_proxy 127.0.0.1:${toString ports.instances}
        }
        @files path /v1/files /v1/files/*
        handle @files {
          uri strip_prefix /v1/files
          reverse_proxy 127.0.0.1:${toString ports.files}
        }
        # The model gateway's own API as the fifth /v1 service segment.
        # Everything under the segment is the gateway's management face
        # (profile reads, the admin family, health, metrics), so the
        # stripped path routes on the management port. Every route on
        # that face carries its own family credential (an admin token or
        # a proxy key) except the unauthenticated health and metrics
        # probes; the edge adds none.
        handle_path /v1/model-gateway/* {
          reverse_proxy 127.0.0.1:${toString config.services.kallipai.polis.model-gateway.port}
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

要用 `kallipctl` 驱动 daemon，把用户加入 daemon 的 socket 组：控制 socket 位于 `/run/kallipai/daemon.sock`，仅对 `kallipai-daemon` 组开放（把 `<username>` 换成你的用户名）：

```nix
users.users."<username>".extraGroups = [ "kallipai-daemon" ];
```

## 部署与验证

在 NixOS Configuration 成功构建并切换到新的 Generation 之后，我们通过下面方式进行验证：

检查六个 unit：

```sh
systemctl status kallipai-daemon kallipai-archeion kallipai-lesche \
  kallipai-files kallipai-instances kallipai-model-gateway
```

然后确认三个域名都有响应：`api.kallipai.lan` 承载平台，`app.kallipai.lan` 承载 web 应用，`model-gw.kallipai.lan` 承载模型网关。部署健康的标志：应用能加载，能注册登录，能创建第一个 agent。网关提供健康端点：

```sh
curl http://model-gw.kallipai.lan/health
```

`/metrics` 指标面在管理端口上，不在 model-gw 域名上：在主机本机访问
`http://127.0.0.1:7500/metrics`，或经 api 边缘访问
`http://api.kallipai.lan/v1/model-gateway/metrics`。它是转发可观测性
计数器的 Prometheus 文本，无需凭据：探针必须在凭据故障时仍可抓取。

NixOS 模块把这个指标面的抓取任务导出为只读列表：
`config.services.kallipai.polis.scrapeConfigs`。导出的任务每 30 秒抓
取一次。模块自身不启动记录器；在主机的 Prometheus 自身选项上接入
该列表（用 `++` 追加更多任务，保留期跟随
`services.prometheus.retentionTime`）：

```nix
services.prometheus = {
  enable = true;
  scrapeConfigs = config.services.kallipai.polis.scrapeConfigs;
};
```

## 网关管理面

模型网关的管理面在同一端口上承载两族凭据：代理键授权分发读与转发面，
平台身份授权 /admin 路由族。两族不共享路径，也不共享失败路径，因此
一支凭据推不出另一支。

/admin 路由族的凭据即平台身份（archeion）。请求要么携带 archeion
admin bearer（机器与 CLI 通道），要么携带本地管理员的平台会话
cookie（浏览器通道：管理页以平台账户登录）。有效的非管理员身份返回
403（已认证但未授权）；凭据无效或缺失返回 401；archeion 不可达时
一律返回 503，按失败关闭处理。未配置 archeion 接线时整个管理面
关闭，分发面与转发面照常运行。

浏览器通道成对加固：会话 cookie 为 HttpOnly 与 SameSite=Strict，且
状态变更请求必须携带跨源浏览器无法伪造的自定义头。会话在服务侧仅存
opaque 哈希。归因以凭据为准：每次管理变更都把操作者写入审计轨迹，
profile 与 set 记录归属账户。

## 管理令牌

`adminTokenFile` 未设置时，archeion 首次启动在状态目录铸造管理令牌（`/var/lib/kallipai/archeion/admin-token.env`，权限 0600），此后只读取、不再改写。用 CLI 读取（本地读文件，不经过服务端）：

```sh
sudo kallipai-admin admin-token show
```

需要时轮换铸造令牌（用当前令牌认证，旧令牌随即失效）。reset 在调用服务端前也会本地读取状态文件，因此需在 archeion 主机上运行；`sudo` 需用 `-E` 保留环境中的令牌：

```sh
export KALLIPAI_ARCHEION_ADMIN_TOKEN=$(sudo kallipai-admin admin-token show)
sudo -E kallipai-admin admin-token reset
```

reset 打印的新值即当前有效令牌，后续命令需重新 export（sudo kallipai-admin admin-token show 再取，或直接捕获 reset 输出）。

设置 `adminTokenFile` 时使用操作者钉住的令牌，轮换会被拒绝。

从铸造切换到钉住：设置 `adminTokenFile` 并重新部署后，archeion 改用钉住的值；状态文件仍留在盘上但已失效，`show` 打印的是旧值，建议删除该文件以免误读。
