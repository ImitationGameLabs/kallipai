//! The GitHub [`OAuthProvider`] implementation.
//!
//! GitHub's stable account id is the numeric `id` (the login handle is
//! renameable). PKCE is supported. See the parent module for the security
//! contract every provider honors.

use serde::Deserialize;
use url::Url;

use super::{OAuthConfig, OAuthError, OAuthProvider, ProviderId, ProviderIdentity, TokenSet};

pub struct GitHubProvider {
    cfg: OAuthConfig,
}

impl GitHubProvider {
    pub fn new(cfg: OAuthConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl OAuthProvider for GitHubProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Github
    }

    fn authorize_url(&self, state: &str, pkce: Option<&str>) -> Url {
        let mut url = Url::parse("https://github.com/login/oauth/authorize").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.cfg.redirect_uri())
            .append_pair("state", state)
            .append_pair("scope", "read:user user:email");
        if let Some(challenge) = pkce {
            url.query_pairs_mut()
                .append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256");
        }
        url
    }

    async fn exchange(
        &self,
        code: &str,
        pkce_verifier: Option<&str>,
        http: &reqwest::Client,
    ) -> Result<TokenSet, OAuthError> {
        let mut form = vec![
            ("client_id", self.cfg.client_id.clone()),
            ("client_secret", self.cfg.client_secret.clone()),
            ("code", code.to_string()),
            ("redirect_uri", self.cfg.redirect_uri()),
        ];
        if let Some(verifier) = pkce_verifier {
            form.push(("code_verifier", verifier.to_string()));
        }
        let resp = http
            .post("https://github.com/login/oauth/access_token")
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError(format!("github token exchange: {status}")));
        }
        // GitHub surfaces errors in-band as 200 + an `error` field.
        #[derive(Deserialize)]
        struct GithubToken {
            access_token: Option<String>,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            error_description: Option<String>,
        }
        let parsed: GithubToken = resp.json().await?;
        match parsed.access_token {
            Some(t) => Ok(TokenSet { access_token: t }),
            None => Err(OAuthError(format!(
                "github token exchange failed: {} {}",
                parsed.error.unwrap_or_else(|| "unknown".to_string()),
                parsed.error_description.unwrap_or_default()
            ))),
        }
    }

    async fn fetch_identity(
        &self,
        tokens: &TokenSet,
        http: &reqwest::Client,
    ) -> Result<ProviderIdentity, OAuthError> {
        #[derive(Deserialize)]
        struct GithubUser {
            id: i64,
            name: Option<String>,
        }
        let resp = http
            .get("https://api.github.com/user")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", tokens.access_token),
            )
            // GitHub requires a User-Agent.
            .header(reqwest::header::USER_AGENT, "kallip-agora")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError(format!("github userinfo: {status}")));
        }
        let u: GithubUser = resp.json().await?;
        Ok(ProviderIdentity {
            subject: u.id.to_string(),
            display_name: u.name,
        })
    }
}
