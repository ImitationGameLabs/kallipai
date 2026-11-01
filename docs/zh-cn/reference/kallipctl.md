---
title: kallipctl CLI 参考手册
description: 面向运维的实例管理工具 CLI 参考。
order: 75
---

`kallipctl` 通过守护进程的控制套接字管理本地 kallip 实例：spawn、adopt、
start/stop/restart、持久化 env、日志与 blob 存储维护。守护进程部署见
[部署文档](../deployment/nixos/minimal.md)；套接字的 `0600` 权限即认证。

## 用个人访问令牌 spawn

远端实例通过一次性注册码接入平台。`kallipctl spawn <slug> <workspace>
  --polis <origin>` 用访问令牌换一个注册码，只把注册码交给实例。两个位置参数
都必填；workspace 写相对路径时按当前目录解析：

```bash
kallipctl spawn dev /srv/kallipai/dev --polis https://kallip.example.com
```

令牌默认读自 `$XDG_CONFIG_HOME/kallipai/access-token`（变量未设时为
`~/.config`），也可用 `--token-file` 指向别处。在 Web 界面创建令牌时要
显式勾选 `archeion:tagma:write`：新令牌默认只读，缺这个串换不到注册码。
令牌文件只允许属主可读：

```bash
chmod 600 ~/.config/kallipai/access-token
```

`--polis` 是平台边缘源（edge origin）：实例把它读作 `KALLIPAI_POLIS_URL`，
换取注册码时也从它拼出 `/v1/archeion`。它不是某个服务的裸地址。取值必须是
http 或 https URL，不带查询串、片段、用户名和密码；明文 http 只接受回环
地址，否则访问令牌和换取到的注册码都会以明文过网。

`--polis` 会把 `KALLIPAI_POLIS_URL` 写进实例环境。再叠加
`-e KALLIPAI_POLIS_URL` 或 `-e KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE` 会被
直接拒绝，不做优先级合并。不带 `--polis` 时不写注册相关变量，离线也可用。

换取注册码发生在 `kallipctl` 里，不在实例里：令牌始终留在运维机器上。
每条失败信息都写明补救动作：

| 失败信息 | 含义 |
| --- | --- |
| no access token at PATH | 令牌文件不存在，创建后写入该路径 |
| token file PATH is readable by other users | `chmod 600` |
| token file PATH is empty | 令牌文件为空，把访问令牌写进去 |
| archeion rejected the access token | 无效、过期或已吊销 |
| the access token lacks archeion:tagma:write | 给令牌补上该字符串 |
| the token belongs to the admin console or a tagma | 换成个人访问令牌再换码 |
| archeion refused the request: 403 MESSAGE | 没有可据以补救的 code，按提示信息排查 |
| too many live pending enrollment codes | 在界面吊销闲置的待注册实例 |
| archeion refused to mint an enrollment code: STATUS MESSAGE | 平台回了失败状态码，后面的文字是平台自己的说明 |
| archeion at ORIGIN did not answer within 30s | 网络可达但一直不应答；30s 是 CLI 内置上限，没有参数可改 |
| cannot reach archeion at ORIGIN | 检查 URL 与网络 |
| archeion answered without an enrollment code | 返回的注册码是空串 |
| minting an enrollment code: DETAIL | 收到的是 2xx，但响应体读不懂，等于没有拿到可用的码 |
| --polis must be an http or https URL | 换取注册码只走 HTTP(S) |
| --polis must be an origin, without a query or fragment | 只写 scheme://host[:port] |
| --polis must be an origin, without a path | 只写 scheme://host[:port] |
| --polis must not carry a user name or password | 去掉地址里的用户名和密码 |
| --polis URL is not loopback and not https | 否则令牌会以明文过网 |

## Shell 补全

补全是动态的：钩子随输入实时向守护进程查询（实例 slug）；
守护进程不可达时自动退化为空候选，不会阻塞输入：

```bash
echo "source <(COMPLETE=bash kallipctl)" >> ~/.bashrc
echo "source <(COMPLETE=zsh kallipctl)" >> ~/.zshrc
```

nix 安装（`kallipctl` 或 `workspace` 包）在构建期生成这些钩子脚本
并自动安装：bash、zsh、fish 落入各 shell 的标准 profile
路径，五种 shell 的脚本同时保留在 `share/kallipai/completions/kallipctl/`
下。每份钩子脚本内嵌其二进制的 nix store 路径，拷贝到其它主机会静默
失效，应在目标主机重新生成。

隐藏的 `generate` 动词输出静态（AOT）脚本：词表在构建期烧定、不查询
守护进程。nix 安装不为 kallipctl 使用它们；适合补全时无法运行 CLI
的封闭环境：

```bash
kallipctl generate bash    # 可选 zsh、fish、elvish、powershell
```
