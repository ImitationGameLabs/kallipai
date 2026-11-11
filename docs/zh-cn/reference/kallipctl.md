---
title: kallipctl CLI 参考手册
description: 面向运维的实例管理工具 CLI 参考。
order: 75
---

`kallipctl` 通过守护进程的控制套接字管理本地 kallip 实例：spawn、adopt、
start/stop/restart、持久化 env、日志与 blob 存储维护；另有本机的平台登记表
（`kallipctl polis`，不依赖守护进程）。守护进程部署见
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
http 或 https URL，不带查询串、片段、用户名和密码；明文 http 只对回环
地址（回环 IP、`localhost`、`*.localhost` 之下的名字）免开关，因为访问令牌
和换取到的注册码都会以明文过网；其它主机上的明文源要加 `--allow-plaintext`，
它是顶层参数，写在子命令之前。`spawn` 带 `--polis` 时、`polis add`、以及每个
会花令牌的 `polis` 子命令都会读它，因为地址规则在碰令牌之前就跑了；不带
`--polis` 的 spawn 用不到它。放行明文源时，命令会先在 stderr 上写明风险。

可信内网里的明文边缘要在子命令前加开关：

```bash
kallipctl --allow-plaintext spawn dev /srv/kallipai/dev --polis http://kallipai.lan:7100
```

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
| --polis URL is not loopback and not https | 令牌会以明文过网；确认网络可信时加 --allow-plaintext |

## 平台登记表

`kallipctl polis` 记录本机要对话的平台，一个平台一个名字，并向平台查询它名下
登记了哪些 tagma。登记表是你自己的文件
`$XDG_CONFIG_HOME/kallipai/polis-registry.toml`，权限 `0600`；这里的动词都不经过
守护进程，所以守护进程停着也能用。

| 动词 | 作用 |
| --- | --- |
| `polis add NAME URL [--replace]` | 按 origin 规范化后记下地址 |
| `polis remove NAME` | 只删这条记录，不动别的 |
| `polis list` | 名字、地址、是否存了令牌；只读文件 |
| `polis token set NAME [--token-file PATH]` | 存下该平台的访问令牌 |
| `polis status NAME` | 平台的判定，以及它名下登记的 tagma |
| `polis revoke NAME TAGMA-ID` | 在平台上吊销一条登记 |

名字沿用实例 slug 的语法：小写字母、数字和 `-`。一个平台一个名字、一个名字一个
平台：给已登记的地址再起一个名字会被拒，把名字改指新地址要加 `--replace`。

地址必须是 origin，规则与 `--polis` 相同（http 或 https，不带 path、query、
fragment 和用户名密码）；非本机回环的明文地址在这里同样要加
`--allow-plaintext`。`list` 只打印掩码后的令牌，密钥本身不会打印。
`token set` 给了 `--token-file` 就读文件，否则读 stdin（终端下隐藏回显），
写入之前先打一次列表请求：平台不认的令牌不会存。

令牌至少要 `archeion:tagma:read`。吊销登记和换注册码（`spawn --polis`）还要
`archeion:tagma:write`，因为新铸的令牌默认只读。`add`、`remove`、`list` 只读写
登记表，不联网。

`status` 每行一条记录，tagma id 在最前，可以直接复制进 `revoke`：

```text
local  http://api.example.com  reachable  token=accepted
tagma-1  enrolled  created=2026-11-01T09:12:00Z
tagma-2  pending  created=2026-11-11T17:40:00Z  expires=2026-11-11T18:10:00Z
```

平台拒收令牌时仍打印 `reachable`，下一行是平台的说明。对一个平台不持有的 id
执行 `revoke` 会打印 `TAGMA is not registered on NAME` 并以 0 退出，因为这正是
它被要求达到的结果。

## Shell 补全

补全是动态的，分两种：实例 slug 随输入实时向守护进程查询；平台名读的是 polis
登记表，不需要守护进程。两者的来源不可用时都退化为空候选，不会阻塞输入：

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
