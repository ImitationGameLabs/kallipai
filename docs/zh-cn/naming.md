---
title: 命名
description: kallipai 这一名字的来历与其使用规则。
order: 70
internal: true
---

本文记录项目名字的来历，以及各书写形式的使用规则。约束仓库内一切名字使用的规则见 `AGENTS.md` 的 [Naming](https://github.com/ImitationGameLabs/kallipai/blob/main/AGENTS.md#naming) 一节。

## kallipai

名字源自 **kallipolis**（希腊语 _kalon_ + _polis_，「美丽之城」），柏拉图《理想国》中的理想城邦。Kallipolis 的设计本意，正是多智能体协作的图景：一个高效和谐的结构，各部件各司其职，整座城如单一有序的有机体般运转。

名字由城邦名的简干 **kallip**（更短、更好打字，去掉不承担技术含义的 `-olis`）加上后缀 **AI** 构成，使项目的性质一目了然。连起来读，`kallipai` 就是字面上的 `kallip` + `ai`。

### 服务名：Archeion 与 Polis

平台的服务名沿用同一希腊城邦意象。**archeion**（ἀρχεῖον）是古代的档案馆——保存公民名册与公共档案的建筑；身份服务（注册、会话、tagma 记录）承担的正是这份职责。**polis**（城）命名部署组合——它把整个平台组装在一起，而非对应某个单一服务。

四个 wire 协议 tag 特意保留 `agora` 拼写（`kallipai-agora-aead-v1`、`kallipai-agora-enroll-v1`、`kallipai-agora-tunnel-proof-v1`、`kallipai-agora-kex-v1`）：它们是域分隔字符串，客户端 SDK 按字节精确匹配，绝不重版本化。

### 单一词干，两个保留名

词干约束一切技术面——crate 名、二进制名、Rust 模块路径、env 前缀（`KALLIPAI_*`）、盘上路径、容器路径与卷、Nix attrs、Cargo/flake `description` 字符串与 User-Agent：在这些面上，一律写 `kallipai`。

两个名字是保留例外：无头 CLI **kallip** 与操作者 CLI **kallipctl** 保持原名——用户敲的命令、它们的参考页（`docs/*/reference/kallip.md`、`docs/*/reference/kallipctl.md`）、以及 CLI 背后的 `crates/kallip` crate。其余一切读作 `kallipai`。

这条规则是为了防止漂移：品牌名渗入标识符，日后就是一次改名的成本；技术词干混入行文，项目读起来就像一个 CLI 旗标。

品牌只有**一种书写形式**：**KallipAI**——用于一切面向人的界面：README 与文档 H1、站点字标、行文中的品牌指称。小写标识符 `kallipai` 覆盖仓库名、`@kallipai` 包作用域、URL 与域名。推荐读音为 **kallipai**（/ˈkælɪpaɪ/），无论品牌以何种形式书写。品牌形式与标识符都不是 CLI 名。

### 网关域词目：collection 与 profile set

模型网关有两层词目。下层是 **profile set**（配置集）：一组有序的模型配置，顺序即故障转移顺序。上层是 **collection**（合集）：可发布的配置集打包单元。该词出现在实体名、路由段（`/user/collections`）与字段名（`collection_name`）中；i18n 键将采用 `collections_*` 词根。合集内的配置集无序；每个配置集保留自己的故障转移顺序。
