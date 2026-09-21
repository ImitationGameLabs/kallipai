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

检查五个 unit：

```sh
systemctl status kallip-daemon kallip-archeion kallip-lesche \
  kallip-files kallip-instances
```

然后确认两个域名都有响应：`api.kallipai.lan` 承载平台，`app.kallipai.lan` 承载 web 应用。部署健康的标志：应用能加载，能注册登录，能创建第一个 agent。

## 管理令牌

`adminTokenFile` 未设置时，archeion 首次启动在状态目录铸造管理令牌（`/var/lib/kallipai/archeion/admin-token.env`，权限 0600），此后只读取、不再改写。用 CLI 读取（本地读文件，不经过服务端）：

```sh
sudo kallip-admin admin-token show
```

需要时轮换铸造令牌（用当前令牌认证，旧令牌随即失效）。reset 在调用服务端前也会本地读取状态文件，因此需在 archeion 主机上运行；`sudo` 需用 `-E` 保留环境中的令牌：

```sh
export KALLIP_ARCHEION_ADMIN_TOKEN=$(sudo kallip-admin admin-token show)
sudo -E kallip-admin admin-token reset
```

reset 打印的新值即当前有效令牌，后续命令需重新 export（sudo kallip-admin admin-token show 再取，或直接捕获 reset 输出）。

设置 `adminTokenFile` 时使用操作者钉住的令牌，轮换会被拒绝。

从铸造切换到钉住：设置 `adminTokenFile` 并重新部署后，archeion 改用钉住的值；状态文件仍留在盘上但已失效，`show` 打印的是旧值，建议删除该文件以免误读。
