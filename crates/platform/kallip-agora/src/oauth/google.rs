//! The Google [`OAuthProvider`] implementation.
//!
//! Google's stable account id is the OIDC `sub`. PKCE is supported; the token
//! endpoint is used with `grant_type=authorization_code`. See the parent module
//! for the security contract every provider honors.

use serde::Deserialize;
use url::Url;

use super::{OAuthConfig, OAuthError, OAuthProvider, ProviderId, ProviderIdentity, TokenSet};

pub struct GoogleProvider {
    cfg: OAuthConfig,
}

impl GoogleProvider {
    pub fn new(cfg: OAuthConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl OAuthProvider for GoogleProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Google
    }

    fn authorize_url(&self, state: &str, pkce: Option<&str>) -> Url {
        let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.cfg.redirect_uri())
            .append_pair("response_type", "code")
            .append_pair("state", state)
            .append_pair("scope", "openid email profile");
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
            ("grant_type", "authorization_code".to_string()),
        ];
        if let Some(verifier) = pkce_verifier {
            form.push(("code_verifier", verifier.to_string()));
        }
        let resp = http
            .post("https://oauth2.googleapis.com/token")
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError(format!("google token exchange: {status}")));
        }
        let parsed: TokenSet = resp.json().await?;
        Ok(parsed)
    }

    async fn fetch_identity(
        &self,
        tokens: &TokenSet,
        http: &reqwest::Client,
    ) -> Result<ProviderIdentity, OAuthError> {
        #[derive(Deserialize)]
        struct GoogleUser {
            sub: String,
            name: Option<String>,
        }
        let resp = http
            .get("https://openidconnect.googleapis.com/v1/userinfo")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", tokens.access_token),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError(format!("google userinfo: {status}")));
        }
        let u: GoogleUser = resp.json().await?;
        Ok(ProviderIdentity {
            subject: u.sub,
            display_name: u.name,
        })
    }
}
