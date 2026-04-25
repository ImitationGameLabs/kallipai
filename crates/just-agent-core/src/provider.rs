use anyhow::{Context, Result, bail};
use just_llm_client::{
    ChatClient, ChatClientOptions, DeepSeekProvider, OpenAiCompatProvider, ProviderRegistry,
};

/// Creates a provider-selected chat client from environment variables.
pub fn client_from_env(system_prompt: &str) -> Result<ChatClient> {
    let provider_id = expect_env("JUST_LLM_PROVIDER")?;
    let model = expect_env("JUST_LLM_MODEL")?;
    let mut registry = ProviderRegistry::new();

    match provider_id.as_str() {
        "deepseek" => {
            let api_key = expect_env("JUST_LLM_DEEPSEEK_API_KEY")?;
            let provider = match std::env::var("JUST_LLM_DEEPSEEK_BASE_URL") {
                Ok(base_url) => {
                    DeepSeekProvider::from_api_key("deepseek", api_key).with_base_url(base_url)
                }
                Err(_) => DeepSeekProvider::from_api_key("deepseek", api_key),
            };
            registry.register(provider);
        }
        "openai-compatible" => {
            let api_key = expect_env("JUST_LLM_OPENAI_COMPAT_API_KEY")?;
            let base_url =
                std::env::var("JUST_LLM_OPENAI_COMPAT_BASE_URL").unwrap_or_else(|_| "".to_string());
            let provider =
                OpenAiCompatProvider::from_api_key("openai-compatible", api_key, base_url);
            registry.register(provider);
        }
        _ => bail!("unsupported JUST_LLM_PROVIDER: {provider_id}"),
    }

    registry
        .chat(
            &provider_id,
            ChatClientOptions::new(model).with_system_prompt(system_prompt),
        )
        .context("failed to create chat client")
}

fn expect_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}
