---
title: 模型配置
description: provider、profile 与 profile set 三层模型，以及每层单独存在的设计原因。
order: 20
---

[配置参考](../index.md)的一部分；本页讲模型配置的三层抽象。

设置写在实例的 `profiles.toml` 里；写法见[模型配置方法](methods.md)。

## 三层模型

模型配置分三层。**Provider** 是一个后端端点：协议族（`family`）、密钥（`api_key`）与可选的 `base_url`，回答“请求发给谁”。**Profile** 把一个模型绑定到一个 provider 端点，并声明该组合的能力：上下文窗口（`max_context_window`）与支持的模态（`modalities`），回答“用哪个模型、它声明了什么”。**Profile set** 是命名的 profile 组，组内构成故障转移链，回答“agent 绑定到哪一组”。

`base_url` 承载模型调用；`model_list_base_url`（可选，默认等于 `base_url`）
承载管理界面拉取的模型列表。列表与调用不在同一 base 的 provider（例如网关把
`/models` 放在另一路径）显式设置它；界面上 provider Test 检查的是这个地址，
而 profile Test 是一次经 `base_url` 的真实调用（provider 解析不到或后端建不起来时除外，那种情况直接判 `invalid_config`）。

三层的关系：agent 不直接绑定单个 profile，而是绑定一个 profile set。root agent 用配置的 `default` set，每个 subagent 在 spawn 时经 `profile_set` 显式声明自己的 set。set 内第一个 profile 是活跃模型，其余构成故障转移链。

每层单独存在都有明确的设计原因。

**Provider 独立成层，因为凭据连同它所服务的端点是一个整体，要换就整体换。** 故障转移不会把单个 profile 改指到另一把密钥：provider 声明了 `api_key`、`family` 与 `base_url`，改动其中任何一项就等于换 provider。共享事实表按 profile 建键：记在某个 profile 上的判定，任何运行该 profile 的 agent 都读得到；同一份凭据上的两条 profile 之间不共享任何判定。把 profile 改指另一个 provider 不会抹掉它上一个 provider 留下的记录——记录留着，直到窗口结束，或某次发送成功把它清掉。

**Profile 独立声明能力，因为故障转移时上下文窗口要跟随新 profile 的声明。** set 内各 profile 的窗口可以不同；切换发生时，上下文按新 profile 声明的窗口处理，预算不变式的校验也依赖这份声明值。窗口因此是 profile 的属性，不是全局常量。

**Agent 绑定的是 set 而非单个 profile，因为故障转移要在 set 内前进。** 活跃 profile 终态失败时，set 内下一个 profile 接手；恢复时活跃索引重置。绑定单个 profile 就没有这条前进路径。

**配置的来源决定谁拥有它：本地文件或模型网关。** local 模式（默认）下，
`profiles.toml` 是唯一事实源，模型配置页面是完整编辑器。model-gateway 模式下，
tagma 启动时从模型网关拉取配置并只保存在内存中：页面变为只读镜像，网关不可达时的
冷启动直接失败，而不是悄悄展示一份过期文件。这个选择记录在 `settings.toml`
（`[profiles.source].mode`），可以在模型配置页面运行期切换。
拉取的内容是本 tagma 选定的那一个 collection（合集）：每个 tagma 指定一个合集供网关提供，
尚未选定合集的 tagma 拉取到的是空配置。

接入 tagma 走它自己的模型配置页：网关标签页把模式切换、合集预览和单选
收在一处。切换会校验平台注册并立刻切换拉取；tagma 不持有需要用户经手的凭据。
没有已注册平台的 daemon 会在标签页的位置显示一张指引卡。网关地址跟随 tagma
所在平台，无需其他配置。

配置文件、字段与运行细节（示例、default 解析、悬空 set、重试预算的细节）见[模型配置方法](methods.md)。
