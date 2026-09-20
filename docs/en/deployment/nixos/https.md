---
title: HTTPS configuration
description: Serve https with Caddy's internal CA where ACME cannot issue, and distribute its trust.
order: 20
---

On a public domain, point the DNS records for `app.<domain>` and
`api.<domain>` at the host; with ports 80 and 443 reachable, Caddy
obtains certificates for your sites automatically. Without reachable
ports 80 and 443, ACME cannot issue certificates. On a private network,
Caddy's `tls internal` directive issues certificates from Caddy's own
local CA instead; browsers show a warning until the host trusts that CA.

Add `tls internal` to each site block in the [Minimal
configuration](minimal.md). A host's block then reads, for example:

```nix
services.caddy.virtualHosts."http://api.kallipai.lan".extraConfig = ''
  # ...the api handle blocks from the Minimal configuration...
  tls internal
'';
```

Repeat for the other host (`app.`)
if both names are to serve https. The internal CA's
root certificate appears after Caddy's first start at
`/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt`
(confirm it with `ls` on the target host). Distribute trust from there:

- Firefox keeps its own trust store, separate from the system bundle;
  import the file manually as an authority (Privacy & Security →
  Certificates → View Certificates → Authorities → Import) and do not
  rely on OS-level trust reaching it.
- For command-line tools (curl, git), copy the root into your
  configuration tree and add it to the system trust:

```sh
cp /var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt \
  /etc/nixos/kallipai-root.crt
```

Adjust the destination to wherever your configuration tree lives; the
`./kallipai-root.crt` reference below resolves next to it.

```nix
security.pki.certificateFiles = [ ./kallipai-root.crt ];
```

The copy exists because the CA generates its root at runtime, while
`security.pki.certificateFiles` is read while the system trust bundle
is built; a direct reference to the `/var/lib` path cannot resolve.
