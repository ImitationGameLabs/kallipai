---
title: HTTPS 配置
description: 在 ACME 无法签发的网络上用 Caddy 的内部 CA 提供 https，并分发其信任。
order: 20
---

公网域名上，把 `app.<domain>` 与 `api.<domain>` 的 DNS 记录指向主机，80、443 端口可达时 Caddy 自动为站点获取证书。80 与 443 端口不可达时，ACME 无法签发证书。私有网络上改用 Caddy 的 `tls internal` 指令，由 Caddy 自己的本地 CA 签发证书；主机信任该 CA 之前，浏览器会显示警告。

在[基础配置](minimal.md)的每个 site block 里加上 `tls internal`。此后某个站点的 block 形如：

```nix
services.caddy.virtualHosts."http://api.kallipai.lan".extraConfig = ''
  # ...the api handle blocks from the Minimal configuration...
  tls internal
'';
```

若两个域名都要走 https，对另一站点（`app.`）重复同样操作。Caddy 首次启动后，内部 CA 的根证书出现在 `/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt`（可在目标主机上用 `ls` 确认）。从这里分发信任：

- Firefox 维护自己的信任库，独立于系统 bundle；把该文件作为权威机构手动导入（Privacy & Security → Certificates → View Certificates → Authorities → Import），不要指望操作系统级信任能传导到它。
- 命令行工具（curl、git）则把根证书复制进你的配置树，加入系统信任：

```sh
cp /var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt \
  /etc/nixos/kallipai-root.crt
```

```nix
security.pki.certificateFiles = [ ./kallipai-root.crt ];
```

之所以要复制一份：CA 的根证书在运行时才生成，而 `security.pki.certificateFiles` 在构建系统信任 bundle 时读取；直接引用 `/var/lib` 路径无法解析。
