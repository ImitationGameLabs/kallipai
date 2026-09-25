//! Timezone-setting method for [`TagmaClient`].

use super::TagmaClient;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct TimezoneResponse {
    timezone: Option<String>,
}

impl TagmaClient {
    /// Fetch the tagma's configured display timezone (an IANA name), or
    /// `None` when unset. Carries its own short timeout — a wedged tagma
    /// must not stall a CLI list render; callers degrade to the machine's
    /// local zone on any error.
    pub async fn get_timezone(&self) -> Result<Option<String>> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url("/settings/timezone"))
                    .timeout(std::time::Duration::from_millis(500)),
            )
            .send()
            .await
            .context("failed to get timezone setting")?;
        let parsed: TimezoneResponse = self
            .handle_response(response, "failed to parse timezone response")
            .await?;
        Ok(parsed.timezone.filter(|s| !s.is_empty()))
    }
}
