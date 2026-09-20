---
title: Model configuration
description: The provider, profile, and profile set layers, and why each layer exists on its own.
order: 20
---

Part of the [Configuration reference](../index.md); this page covers the three-layer model behind model configuration.

The provider-selection variables: [Tagma](../../reference/environment-variables/tagma.md).

## The Three-Layer Model

Model configuration has three layers. A **provider** is a backend endpoint: protocol family (`family`), key (`api_key`), and an optional `base_url`; it answers who receives the request. A **profile** binds one model to a provider endpoint and declares that combination's capabilities: the context window (`max_context_window`) and supported modalities (`modalities`); it answers which model is used and what it declares. A **profile set** is a named group of profiles forming a failover chain; it answers which group an agent is bound to.

The layers relate like this: an agent never binds a single profile; it binds a profile set. The root agent uses the config's `default` set, and every subagent declares its own set at spawn time via `profile_set`. The set's first profile is the active model; the rest form the failover chain.

Each layer exists on its own for a concrete reason.

**Provider is its own layer because rate limits and the retry budget are endpoint-scoped.** Rate limiting is an endpoint-level behavior: several profiles sharing one endpoint share one retry budget instead of each keeping its own. Gathering endpoint details into the provider makes the budget's ownership land on this layer naturally.

**A profile declares its capabilities because the context window follows the new profile's declaration on failover.** Windows may differ within a set; when a switch happens, the context is handled against the newly active profile's declared window, and budget-invariant checks rely on that declared value. The window is therefore a property of the profile, not a global constant.

**An agent binds a set, not a single profile, because failover advances within the set.** When the active profile fails terminally, the next profile in the set takes over; the active index resets on restore. Binding a single profile would leave no path to advance.

Config files, fields, and runtime details (examples, default resolution, dangling sets, retry-budget specifics) live in [Model configuration methods](methods.md).
