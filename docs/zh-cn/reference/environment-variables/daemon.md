---
title: 本地 daemon
description: 本地 kallipai-daemon 读取的环境变量。
order: 60
---

[环境变量参考](index.md)的一部分；本页覆盖桌面 daemon。

| 变量                                       | 必填        | 默认                                        | 说明                                                                                                                                                                                                                                                                                                                                                                        |
| ------------------------------------------ | ----------- | ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIPAI_DAEMON_RECORD_DIR`               | 否          | `~/.local/state/kallipai/daemon/instances/` | daemon 的注册记录根：每个受管实例一条 `<slug>.json` 记录（实例 id、属主 uid、目标 uid、workspace、用户环境、启动锚点，以及该次启动解析出的数据、配置、状态三个根）。凡需要这些树的读取方都读记录：日志列表指向记录中的状态根，重启则先把环境解析出的根与记录的数据根比对。旧 daemon 写下、未存根的记录在启动时补齐一次；补不齐的记录保持原样，需要该根的命令拒绝并指名可补上它的键。逐字设置；覆盖 XDG state-home 推导。 |
| `KALLIPAI_TAGMA_SLUG`                      | 是（tagma） | _（无）_                                    | 命名实例；tagma 从它解析数据、配置、状态三个根（见下表：数据根与配置根是各自 home 下的同名树，状态根是 `<state home>/kallipai`，slug 只出现在日志叶子上），缺失则拒绝启动。slug 语法：`[a-z0-9][a-z0-9-]*`，至多 64 字符。受管实例由 daemon 注入；直跑时自行设置（compose 设 `main`）。 |
| `KALLIPAI_TAGMA_DATA_DIR`                  | 否          | `<data home>/kallipai/tagmata/<slug>`       | 实例数据根。设置后逐字采用，必须为绝对路径，并优先于 `XDG_DATA_HOME`；未设置或为空时根为 `<XDG_DATA_HOME>/kallipai/tagmata/<slug>`，该变量也未设或为空时为 `<home>/.local/share/kallipai/tagmata/<slug>`。相对路径被拒绝且报错指名该键；没有任何规则给出根时同样拒绝启动。受管启动只把 daemon 自己已有的值传下去，不自行拟定根；落下式启动时根跟随目标用户的 home。请求无法设置它。 |
| `KALLIPAI_TAGMA_STATE_DIR`                 | 否          | `<state home>/kallipai`                     | 实例状态根；日志落在 `<state root>/tagmata/<slug>/logs`。解析顺序同上面的行：该键为绝对路径时逐字采用，否则 `<XDG_STATE_HOME>/kallipai`，再否则 `<home>/.local/state/kallipai`。daemon 自己的记录根同样取自 state home，所以就地形态下实例日志与 daemon 记录相邻，落下式启动时跟随目标用户。请求无法设置它。 |
| `KALLIPAI_TAGMA_CONFIG_DIR`                | 否          | `<config home>/kallipai/tagmata/<slug>`     | 实例配置根（`profiles.toml`、`exec_hooks.toml`、`settings.toml`）。解析方式与数据根相同：该键为绝对路径时逐字采用，否则 `<XDG_CONFIG_HOME>/kallipai/tagmata/<slug>`，再否则 `<home>/.config/kallipai/tagmata/<slug>`。解析结果变化时新旧两棵树之间不做迁移。相对路径被拒绝并指名该键。请求无法设置它。 |
| `KALLIPAI_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT` | 否          | 未设                                        | tagma real-root 防护唯一的逃生口。以初始 user namespace 中 euid 0 启动的 tagma（主机真实 root；映射沙箱 root 位于嵌套 namespace，不受影响）拒绝启动。仅精确值 `1` 解锁启动并记一条警告；其他任何值（空、`0`、任意）都保持拒绝。面向无 user namespace 重映射的有根容器；其他场景请用专属无特权用户。                                                                         |
| `KALLIPAI_HARVEST_BASH`                    | 否          | `/bin/bash`                                 | 登录环境采集所用的 bash（两种启动形态；落下式启动时以目标用户运行）。这是管理常量：没有 `/bin/bash` 的部署（NixOS）把它指向受管 bash；请求或用户环境供给的任何值都动不了它。                                                                                                                                                                                                |
| `KALLIPAI_DAEMON_SOCKET_GROUP`             | 否          | _（无）_                                    | 控制 socket 访问组：设置后，daemon 在绑定时把 socket 交给该组（经组数据库解析）并把模式放宽到 0660，组成员即可驱动 daemon。                                                                                                                                                                                                                                                 |

落下式启动（root daemon 为声明用户启动实例）时，daemon 依据 passwd 条目构造实例的标准身份环境：`HOME`、`USER`、`LOGNAME`、`SHELL`、`XDG_CONFIG_HOME`、`XDG_DATA_HOME`、`XDG_STATE_HOME`、`XDG_CACHE_HOME`，以及运行时目录存在时的 `XDG_RUNTIME_DIR`。这些键由 daemon 所有：采集到的登录环境与请求对都无法覆盖它们。实例的三个根按这些定值解析，因此落在目标用户的 home 里；部署在 daemon 上设置的根键在这一形态下被丢弃。除身份定值外，实例还继承部署的根变量与下方所述的追踪配置。

登录环境采集本身也会播种 `XDG_RUNTIME_DIR`（用户运行时目录存在时），使从它推导的系统会话脚本与真实登录同形求值。

就地启动时，daemon 只传递自己环境里确实存在的根变量：三个自有键以及 `XDG_DATA_HOME`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`。它不自行拟定任何根。部署在 daemon 单元里写了某个根，该根就固定到 daemon 启动的每个实例；一个根都没写时，各实例按自己的 `HOME` 解析。登录采集导出的根键不进实例环境：实例用户自己拥有的 profile 文件不能决定实例落在哪棵树。其余键上部署优先于 profile，profile 优先于种子。三个根的解析顺序一致：自有键、XDG 变量、`$HOME`。空值跳过，相对路径的 XDG 值被忽略，相对路径的自有键被拒绝并指名该键。没有任何规则给出根时，tagma 拒绝启动，报错指名可补上该根的键或变量。

追踪配置是 daemon 唯一转交的一族变量。daemon 自身环境带有
`OTEL_EXPORTER_OTLP_ENDPOINT` 时，它启动的每个实例都收到同一端点，并在设置
时一并收到 `OTEL_TRACES_SAMPLER` 与 `OTEL_TRACES_SAMPLER_ARG`。实例标识看的
是通用键 `OTEL_EXPORTER_OTLP_ENDPOINT`、不是它的来源：实例自己的环境里带这个
键（daemon 给的，或登录 shell 导出的），就得到
`OTEL_RESOURCE_ATTRIBUTES=service.instance.id=<slug>`，使同一构建的多个实例在
收集器里互不混淆；没有这个键的实例不加该属性。同名键以
daemon 的值为准，请求无法指名这些键：允许清单只收 `KALLIPAI_*`、`RUST_LOG`
与 `PATH`。其他渠道设置的 `OTEL_RESOURCE_ATTRIBUTES` 按合并处理：原有属性保
留，`service.instance.id` 追加在后。主机上实例多时，把 `OTEL_TRACES_SAMPLER`
设为 `parentbased_traceidratio`、`OTEL_TRACES_SAMPLER_ARG` 设为一个小数：每
个实例各带一套批处理导出器，全量采样只是白烧 CPU。

记录里存着每次启动解析出的三个根。重启时若环境解析出的数据根不再是记录
中的那个，daemon 拒绝拉起进程并打印两条路径：否则实例会在 daemon 不记录、
不扫描、也不按它停机的树里读写与落日志。若这次移动来自旧 daemon 写下的记录
（当时登录 profile 的根键可以穿透），把该键写到 daemon 单元并重启 daemon，
或把实例落到的树 adopt 进来，让记录指向它。
