---
title: Configuration
description: How the platform is configured, from environment variables to the model profiles file and the dev stack.
order: 10
---

Runtime configuration is environment-first: every component reads its settings from
`KALLIPAI_*` variables at startup. The LLM backend is configured through the
instance's `profiles.toml`, and NixOS deployments configure services through
[module options](../deployment/nixos/index.md); the file's shape is in
[Model configuration methods](tagma/methods.md).

Three example files ship: the root `.env.example` for the tagma itself,
`compose/.env.example` for the stack, and `compose/.env.prod.example` for the
service env file a prod deployment names in `KALLIPAI_ENV_FILE`. A dev setup
copies the first two to `.env` beside them and fills in the required values;
`direnv` loads both through `.envrc`.

The tagma configuration reference lives at [Tagma](tagma/index.md); environment
variables for the other components (cron, the files service, the local daemon)
are in the [environment variables reference](../reference/environment-variables/index.md).

## Dev Stack Shape

Three variables shape the dev stack's edge and the web dev server together,
three more replace what the edge serves, and one names the origin the browser
types (all flow from `compose/.env`, which direnv reads):

| Variable        | Default                                     | Purpose                                                                                                                         |
| --------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIPAI_EDGE_TLS` | `on` | HMR protocol for `deno task dev`: `on` = `wss`, for the TLS edge in front; `off` = `ws`, for a plain-http edge. It has no say in which entry the container runs. |
| `KALLIPAI_EDGE_PORT` | `443` | The port the dev edge listens on. The compose stack writes it into the edge's site address (`*.<domain>:<port>`) and the API base. The browser-facing origin is `https://app.<domain>`, so a start on any other port has to name `KALLIPAI_APP_ORIGIN`. Vite uses it as its HMR client port. |
| `KALLIPAI_APP_ORIGIN` | `https://app.<domain>` | The origin the browser types for the app: CORS, the OAuth return base and the WebAuthn relying party compare it verbatim. `compose.dev.yaml` derives it from `KALLIPAI_DOMAIN` for an https edge on 443; name it when the edge answers under another host, off 443, or over http; a value you write is used as written, the scripts do not compare it. |
| `KALLIPAI_DOMAIN` | `kallipai.com` | The domain the edge site, the API base and the browser-facing values derive from; the web app's URLs derive in the browser at runtime. |
| `KALLIPAI_EDGE_ENTRY` | `/etc/caddy/Caddyfile.vite` | The entry the dev edge container runs. `/etc/caddy/Caddyfile.plain` serves the image's bundle over http and mounts no certificate; prod runs the image's own `Caddyfile`. |
| `KALLIPAI_EDGE_SITE` | `*.<domain>:<port>` | Replaces the edge's site address. On the https edge a port written here has to equal `KALLIPAI_EDGE_PORT`, because the API base and the HMR client port follow that value, and an address naming no port answers on 443, so a port is required whenever `KALLIPAI_EDGE_PORT` names another one; the scripts refuse either mismatch. The plain entry carries its own port in this address and sits outside that check, and it needs an `http://` address. |
| `KALLIPAI_API_BASE` | `https://api.<domain>:<port>` | The API origin the browser is given, and the address the edge serves it on: one value, so the two cannot disagree. |
| `KALLIPAI_MODEL_GW_SITE` | `model-gw.<domain>:<port>` | The host the model gateway's forwarding face answers on. The edge strips one `/v1` prefix there, so the base the gateway publishes ends in `/v1`; the management face stays on the api host. The plain entry's scheme rule covers this address like the other two. |
