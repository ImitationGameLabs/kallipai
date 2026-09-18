use clap::Parser;

/// CLI arguments for `kallip-model-gateway`.
///
/// The gateway owns the model profile registry and the credential store in its
/// own Postgres; distribution reads and credential injection both resolve
/// through that store. Configuration follows the service convention: every
/// knob has a `KALLIP_MODEL_GATEWAY_*` env var, and the database URL is
/// required so a missing store fails fast at boot.
#[derive(Parser)]
#[command(
    name = "kallip-model-gateway",
    version,
    about = "kallip model gateway: profiles distribution + credential-injecting forwarding"
)]
pub struct Args {
    /// Data-plane address to listen on (behind a TLS-terminating
    /// reverse proxy): distribution reads, forwarding, and health.
    /// Default is 7501: the platform's managed services hold the 7x00
    /// range (7100 archeion, 7200 lesche, 7300 instances, 7400 files),
    /// and those are management faces, so the LLM-compatible
    /// forwarding plane takes the adjacent 7x01.
    #[arg(
        long,
        env = "KALLIP_MODEL_GATEWAY_ADDR",
        default_value = "127.0.0.1:7501"
    )]
    pub listen_addr: String,
    /// Management-plane address (the /admin family). Binds the loopback
    /// by default: the admin namespace is house-only and is not
    /// published next to the LLM-compatible data plane. The default
    /// 7500 aligns the management face with the platform's managed
    /// services (7100 archeion, 7200 lesche, 7300 instances, 7400
    /// files): one api namespace, one port family.
    #[arg(
        long,
        env = "KALLIP_MODEL_GATEWAY_MANAGEMENT_ADDR",
        default_value = "127.0.0.1:7500"
    )]
    pub management_listen_addr: String,
    /// Comma-separated CORS allowed origins (the app's origin(s)). Empty = no
    /// cross-origin allowed. Never use a wildcard on a public-facing deploy.
    #[arg(long, env = "KALLIP_MODEL_GATEWAY_CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// The gateway's own base URL as clients should reach it -- the
    /// `base_url` field of every distributed profile (the design doc's
    /// "base_url = the proxy itself"; tagmas point their provider config
    /// here). Includes the `/v1` prefix: consumers append
    /// `chat/completions` to this base.
    #[arg(
        long,
        env = "KALLIP_MODEL_GATEWAY_PUBLIC_BASE_URL",
        default_value = "http://127.0.0.1:7501/v1"
    )]
    pub public_base_url: String,
    /// Postgres URL for the gateway's durable store (e.g.
    /// `postgres://user:pass@host/db`). Required: the registry and the
    /// credential store are the gateway's reason to exist, so a missing URL
    /// fails fast at boot rather than silently running stateless.
    #[arg(long, env = "KALLIP_MODEL_GATEWAY_DATABASE_URL")]
    pub database_url: String,
    /// Static management token for the admin face (sent as
    /// `Authorization: Bearer <token>`). Read once at boot; rotating it
    /// means restarting the process. Unset (empty) closes the management
    /// face: every admin request is refused with 401 while the
    /// distribution and forwarding faces run unaffected.
    #[arg(
        long,
        env = "KALLIP_MODEL_GATEWAY_MANAGEMENT_TOKEN",
        default_value = ""
    )]
    pub management_token: String,
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    /// Pins the port-family defaults: the data plane owns the 7x01
    /// slot, the management face keeps the platform's managed-services
    /// port 7x00, and the published base URL advertises the data
    /// plane. These assertions keep an accidental flip of either
    /// plane's port from landing silently.
    #[test]
    fn defaults_pin_the_port_split() {
        for var in [
            "KALLIP_MODEL_GATEWAY_ADDR",
            "KALLIP_MODEL_GATEWAY_MANAGEMENT_ADDR",
            "KALLIP_MODEL_GATEWAY_PUBLIC_BASE_URL",
        ] {
            // SAFETY: single-threaded test setup, and nothing else in
            // this crate reads these vars, so no concurrent access.
            unsafe { std::env::remove_var(var) };
        }
        let args = Args::parse_from([
            "kallip-model-gateway",
            "--database-url",
            "postgres://user:pass@host/db",
        ]);
        assert_eq!(args.listen_addr, "127.0.0.1:7501");
        assert_eq!(args.management_listen_addr, "127.0.0.1:7500");
        assert_eq!(args.public_base_url, "http://127.0.0.1:7501/v1");
        // The two faces must stay on distinct ports.
        assert_ne!(args.listen_addr, args.management_listen_addr);
    }
}
