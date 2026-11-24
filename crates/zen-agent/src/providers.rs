use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiStandard {
    OpenAi,

    Anthropic,
}

impl ApiStandard {
    pub fn as_str(self) -> &'static str {
        match self {
            ApiStandard::OpenAi => "openai",
            ApiStandard::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub key: &'static str,

    pub openai_base_url: Option<&'static str>,

    pub anthropic_base_url: Option<&'static str>,

    pub api_key_env: Option<&'static str>,

    pub model_ids: &'static [&'static str],

    pub pricing: &'static [(&'static str, f64, f64)],

    pub featured: &'static [&'static str],

    pub rate_limit_notes: &'static str,
}

impl ProviderInfo {
    pub fn endpoint(&self, anthropic_available: bool) -> Option<(ApiStandard, &'static str)> {
        if anthropic_available && let Some(url) = self.anthropic_base_url {
            return Some((ApiStandard::Anthropic, url));
        }
        self.openai_base_url.map(|url| (ApiStandard::OpenAi, url))
    }

    pub fn price_for(&self, model: &str) -> Option<(f64, f64)> {
        self.pricing
            .iter()
            .find(|(m, _, _)| *m == model)
            .map(|(_, i, o)| (*i, *o))
    }

    pub fn endpoint_resolved(&self, anthropic_available: bool) -> Option<(ApiStandard, String)> {
        let (standard, url) = self.endpoint(anthropic_available)?;
        Some((standard, resolve_url_template(url)?))
    }
}

pub fn resolve_url_template(url: &str) -> Option<String> {
    if !url.contains('{') {
        return Some(url.to_string());
    }
    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID")
        .ok()
        .filter(|v| !v.is_empty())?;
    Some(url.replace("{ACCOUNT_ID}", &account_id))
}

pub fn provider_registry() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            key: "openrouter",
            openai_base_url: Some("https://openrouter.ai/api/v1"),
            anthropic_base_url: None,
            api_key_env: Some("OPENROUTER_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &["qwen/qwen3-coder:free"],
            rate_limit_notes: "Rate limits vary by model and tier; free models are heavily throttled.",
        },
        ProviderInfo {
            key: "groq",
            openai_base_url: Some("https://api.groq.com/openai/v1"),
            anthropic_base_url: None,
            api_key_env: Some("GROQ_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Generous free tier with per-minute TPM caps.",
        },
        ProviderInfo {
            key: "anthropic",
            openai_base_url: None,
            anthropic_base_url: Some("https://api.anthropic.com"),
            api_key_env: Some("ANTHROPIC_API_KEY"),
            model_ids: &["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Tiered RPM/TPM; 429 = backoff, 529 = overloaded (retry with backoff).",
        },
        ProviderInfo {
            key: "openai",
            openai_base_url: Some("https://api.openai.com/v1"),
            anthropic_base_url: None,
            api_key_env: Some("OPENAI_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Tiered usage limits; respect Retry-After.",
        },
        ProviderInfo {
            key: "ollama",
            openai_base_url: Some("http://localhost:11434/v1"),
            anthropic_base_url: None,
            api_key_env: None,
            model_ids: &[],
            pricing: &[],
            featured: &["qwen3-coder", "qwen3-coder:30b"],
            rate_limit_notes: "Local - no rate limits, no pacing needed.",
        },
        ProviderInfo {
            key: "zai",
            openai_base_url: Some("https://api.z.ai/api/paas/v4"),
            anthropic_base_url: Some("https://api.z.ai/api/anthropic"),
            api_key_env: Some("ZAI_API_KEY"),
            model_ids: &["glm-5.2", "glm-4.6", "glm-4.5", "glm-4.5-air"],

            pricing: &[("glm-5.2", 1.40, 4.40)],
            featured: &["glm-5.2"],
            rate_limit_notes: "Tiered RPM/TPM; free 5-day trial tier is strict (5M tokens/day).",
        },
        ProviderInfo {
            key: "moonshot",
            openai_base_url: Some("https://api.moonshot.ai/v1"),
            anthropic_base_url: Some("https://api.moonshot.ai/anthropic"),
            api_key_env: Some("MOONSHOT_API_KEY"),
            model_ids: &["kimi-k2.7", "kimi-k3", "kimi-k2.6"],

            pricing: &[("kimi-k2.7", 0.95, 4.00), ("kimi-k3", 3.00, 15.00)],
            featured: &["kimi-k2.7"],
            rate_limit_notes: "Tiered limits; Kimi For Coding plan users route via the same endpoint.",
        },
        ProviderInfo {
            key: "deepseek",
            openai_base_url: Some("https://api.deepseek.com/v1"),
            anthropic_base_url: Some("https://api.deepseek.com/anthropic"),
            api_key_env: Some("DEEPSEEK_API_KEY"),
            model_ids: &["deepseek-v4-flash", "deepseek-chat", "deepseek-reasoner"],

            pricing: &[("deepseek-v4-flash", 0.14, 0.28)],
            featured: &["deepseek-v4-flash"],
            rate_limit_notes: "Aggressive rate limits on lower tiers; 429s are common - \
                back off with jitter, never hammer (account-safety PR #47 applies).",
        },
        ProviderInfo {
            key: "cloudflare",

            openai_base_url: Some(
                "https://api.cloudflare.com/client/v4/accounts/{ACCOUNT_ID}/ai/v1",
            ),
            anthropic_base_url: None,
            api_key_env: Some("CLOUDFLARE_API_TOKEN"),
            model_ids: &[
                "@cf/meta/llama-3.1-8b-instruct",
                "@cf/cloudflare/clef-flash",
            ],

            pricing: &[("@cf/cloudflare/clef-flash", 0.09, 0.0)],

            featured: &[
                "@cf/cloudflare/clef-flash",
                "@cf/meta/llama-3.1-8b-instruct",
            ],
            rate_limit_notes: "Free tier: 10,000 Neurons/day (resets 00:00 UTC), \
                then $0.011/1K Neurons. Best for small tasks (classification, \
                routing, summarisation) - not a coding-model replacement.",
        },
        ProviderInfo {
            key: "together",
            openai_base_url: Some("https://api.together.xyz/v1"),
            anthropic_base_url: None,
            api_key_env: Some("TOGETHER_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Tiered limits.",
        },
        ProviderInfo {
            key: "mistral",
            openai_base_url: Some("https://api.mistral.ai/v1"),
            anthropic_base_url: None,
            api_key_env: Some("MISTRAL_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Tiered limits.",
        },
        ProviderInfo {
            key: "cerebras",
            openai_base_url: Some("https://api.cerebras.ai/v1"),
            anthropic_base_url: None,
            api_key_env: Some("CEREBRAS_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Very high speed; free tier throttled.",
        },
        ProviderInfo {
            key: "fireworks",
            openai_base_url: Some("https://api.fireworks.ai/inference/v1"),
            anthropic_base_url: None,
            api_key_env: Some("FIREWORKS_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Tiered limits.",
        },
        ProviderInfo {
            key: "google",
            openai_base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            anthropic_base_url: None,
            api_key_env: Some("GOOGLE_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Free tier is quota-gated rather than ban-heavy.",
        },
        ProviderInfo {
            key: "gemini",
            openai_base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            anthropic_base_url: None,
            api_key_env: Some("GOOGLE_API_KEY"),
            model_ids: &[],
            pricing: &[],
            featured: &[],
            rate_limit_notes: "Alias of `google`.",
        },
    ]
}

pub fn find_provider(key: &str) -> Option<ProviderInfo> {
    let key = key.to_ascii_lowercase();
    provider_registry().into_iter().find(|p| p.key == key)
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub provider: String,

    pub model: String,

    pub api_standard: ApiStandard,

    pub base_url: String,

    pub api_key_env: Option<String>,
}

impl ModelSpec {
    /// Parse `provider/model` into a resolved spec.
    ///
    /// A bare model (no `/`) and an unknown provider keep the old
    /// whole-spec default: the caller never named a provider we know,
    /// so there is nothing to resolve. A *known* provider whose
    /// endpoint cannot resolve is different: the user explicitly chose
    /// that provider, so silently sending the whole spec to the default
    /// gateway would discard the provider, model and key env and fail
    /// later with a confusing model-not-found or 401. That case is an
    /// error naming what the provider needs (issue #346).
    pub fn parse(
        spec: &str,
        default_base_url: &str,
        anthropic_available: bool,
    ) -> Result<Self, String> {
        let spec = spec.trim();
        match spec.split_once('/') {
            Some((provider, model)) => {
                let key = provider.to_ascii_lowercase();
                if let Some(info) = find_provider(&key) {
                    return match info.endpoint_resolved(anthropic_available) {
                        Some((standard, url)) => Ok(Self {
                            provider: key,
                            model: model.to_string(),
                            api_standard: standard,
                            base_url: url,
                            api_key_env: info.api_key_env.map(str::to_string),
                        }),
                        None => Err(resolution_error(&info, anthropic_available)),
                    };
                }

                Ok(Self {
                    provider: "default".to_string(),
                    model: spec.to_string(),
                    api_standard: ApiStandard::OpenAi,
                    base_url: default_base_url.to_string(),
                    api_key_env: None,
                })
            }
            None => Ok(Self {
                provider: "default".to_string(),
                model: spec.to_string(),
                api_standard: ApiStandard::OpenAi,
                base_url: default_base_url.to_string(),
                api_key_env: None,
            }),
        }
    }

    pub fn api_key(&self, ambient: Option<&str>) -> Option<String> {
        if let Some(env) = &self.api_key_env
            && let Ok(v) = std::env::var(env)
            && !v.is_empty()
        {
            return Some(v);
        }
        ambient.map(str::to_string)
    }
}

/// Why a known provider's endpoint did not resolve, for the
/// `ModelSpec::parse` error (issue #346). Two cases exist: the provider
/// has no usable endpoint at all (Anthropic-only provider without an
/// Anthropic client), or its endpoint URL is a template that cannot be
/// resolved (today only `{ACCOUNT_ID}`, filled from
/// `CLOUDFLARE_ACCOUNT_ID`).
fn resolution_error(info: &ProviderInfo, anthropic_available: bool) -> String {
    if info.endpoint(anthropic_available).is_none() {
        format!(
            "provider '{}' has no usable endpoint: it has no OpenAI-compatible endpoint and no Anthropic-compatible client is available",
            info.key
        )
    } else {
        format!(
            "provider '{}' cannot resolve its endpoint URL: CLOUDFLARE_ACCOUNT_ID is not set or empty",
            info.key
        )
    }
}

pub fn model_pricing(provider_key: &str, model: &str) -> Option<(f64, f64)> {
    find_provider(provider_key)?.price_for(model)
}

pub fn call_cost_usd(input_tokens: u64, output_tokens: u64, pricing: (f64, f64)) -> f64 {
    (input_tokens as f64) * pricing.0 / 1_000_000.0
        + (output_tokens as f64) * pricing.1 / 1_000_000.0
}

pub fn featured_models() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for info in provider_registry() {
        for m in info.featured {
            out.push((info.key.to_string(), m.to_string()));
        }
    }
    out
}

pub fn authable_providers() -> Vec<ProviderInfo> {
    let preferred_order = [
        "deepseek",
        "zai",
        "moonshot",
        "cloudflare",
        "openrouter",
        "groq",
        "anthropic",
        "openai",
        "google",
        "together",
        "mistral",
        "cerebras",
        "fireworks",
    ];
    let reg: HashMap<&str, ProviderInfo> = provider_registry()
        .into_iter()
        .map(|p| (p.key, p))
        .collect();
    preferred_order
        .into_iter()
        .filter_map(|k| reg.get(k).cloned())
        .filter(|p| p.api_key_env.is_some())
        .collect()
}

pub fn cheap_first_order() -> Vec<String> {
    let mut priced: Vec<(&str, f64)> = Vec::new();
    let mut unpriced: Vec<&str> = Vec::new();
    for info in provider_registry() {
        let cheapest = info
            .pricing
            .iter()
            .map(|(_, i, _)| *i)
            .fold(None, |a: Option<f64>, b| {
                Some(a.map_or(b, |x: f64| x.min(b)))
            });
        match cheapest {
            Some(p) => priced.push((info.key, p)),
            None => unpriced.push(info.key),
        }
    }
    priced.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    priced
        .into_iter()
        .map(|(k, _)| k.to_string())
        .chain(unpriced.into_iter().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_api_key_env_names_are_all_in_strip_list() {
        // Guards the sync between provider_registry() and
        // zen_core::PROVIDER_API_KEY_ENV_NAMES: every key a provider may use
        // must be stripped from MCP stdio children (issue #378). Adding a
        // provider with a new key env without updating the strip list fails
        // here instead of leaking the key to a third-party MCP server.
        for p in provider_registry() {
            if let Some(env) = p.api_key_env {
                assert!(
                    zen_core::PROVIDER_API_KEY_ENV_NAMES.contains(&env),
                    "provider '{}' uses api_key_env '{env}' missing from zen_core::PROVIDER_API_KEY_ENV_NAMES",
                    p.key
                );
            }
        }
    }

    #[test]
    fn registry_has_fifteen_providers() {
        assert_eq!(provider_registry().len(), 15);
    }

    #[test]
    fn chinese_providers_have_anthropic_endpoints() {
        for key in ["zai", "moonshot", "deepseek"] {
            let p = find_provider(key).unwrap_or_else(|| panic!("missing provider {key}"));
            assert!(
                p.anthropic_base_url.is_some(),
                "{key} must document an Anthropic-compatible endpoint"
            );
            assert!(
                p.openai_base_url.is_some(),
                "{key} must document an OpenAI-compatible endpoint"
            );
        }
    }

    #[test]
    fn zai_endpoints_are_correct() {
        let p = find_provider("zai").unwrap();
        assert_eq!(p.anthropic_base_url, Some("https://api.z.ai/api/anthropic"));
        assert_eq!(p.openai_base_url, Some("https://api.z.ai/api/paas/v4"));
        assert_eq!(p.api_key_env, Some("ZAI_API_KEY"));
    }

    #[test]
    fn moonshot_endpoints_are_correct() {
        let p = find_provider("moonshot").unwrap();
        assert_eq!(
            p.anthropic_base_url,
            Some("https://api.moonshot.ai/anthropic")
        );
        assert_eq!(p.openai_base_url, Some("https://api.moonshot.ai/v1"));
        assert_eq!(p.api_key_env, Some("MOONSHOT_API_KEY"));
    }

    #[test]
    fn deepseek_endpoints_are_correct() {
        let p = find_provider("deepseek").unwrap();
        assert_eq!(
            p.anthropic_base_url,
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(p.openai_base_url, Some("https://api.deepseek.com/v1"));
        assert_eq!(p.api_key_env, Some("DEEPSEEK_API_KEY"));
    }

    #[test]
    fn endpoint_prefers_anthropic_when_available() {
        let p = find_provider("zai").unwrap();
        let (std, url) = p.endpoint(true).unwrap();
        assert_eq!(std, ApiStandard::Anthropic);
        assert_eq!(url, "https://api.z.ai/api/anthropic");
    }

    #[test]
    fn endpoint_falls_back_to_openai() {
        let p = find_provider("zai").unwrap();
        let (std, url) = p.endpoint(false).unwrap();
        assert_eq!(std, ApiStandard::OpenAi);
        assert_eq!(url, "https://api.z.ai/api/paas/v4");
    }

    #[test]
    fn anthropic_only_provider_has_no_openai_fallback() {
        let p = find_provider("anthropic").unwrap();
        assert!(p.endpoint(false).is_none());
        assert!(p.endpoint(true).is_some());
    }

    #[test]
    fn parse_zai_spec() {
        let s =
            ModelSpec::parse("zai/glm-5.2", "http://default/v1", false).expect("parse resolves");
        assert_eq!(s.provider, "zai");
        assert_eq!(s.model, "glm-5.2");
        assert_eq!(s.api_standard, ApiStandard::OpenAi);
        assert_eq!(s.base_url, "https://api.z.ai/api/paas/v4");
        assert_eq!(s.api_key_env.as_deref(), Some("ZAI_API_KEY"));
    }

    #[test]
    fn parse_zai_spec_anthropic_client() {
        let s = ModelSpec::parse("zai/glm-5.2", "http://default/v1", true).expect("parse resolves");
        assert_eq!(s.api_standard, ApiStandard::Anthropic);
        assert_eq!(s.base_url, "https://api.z.ai/api/anthropic");

        assert_eq!(s.model, "glm-5.2");
    }

    #[test]
    fn parse_moonshot_and_deepseek_specs() {
        let s = ModelSpec::parse("moonshot/kimi-k2.7", "http://default/v1", false)
            .expect("parse resolves");
        assert_eq!(s.provider, "moonshot");
        assert_eq!(s.model, "kimi-k2.7");
        assert_eq!(s.base_url, "https://api.moonshot.ai/v1");

        let s = ModelSpec::parse("deepseek/deepseek-v4-flash", "http://default/v1", false)
            .expect("parse resolves");
        assert_eq!(s.provider, "deepseek");
        assert_eq!(s.model, "deepseek-v4-flash");
        assert_eq!(s.base_url, "https://api.deepseek.com/v1");
    }

    #[test]
    fn parse_is_case_insensitive() {
        let s =
            ModelSpec::parse("ZAI/glm-5.2", "http://default/v1", false).expect("parse resolves");
        assert_eq!(s.provider, "zai");
        assert_eq!(s.model, "glm-5.2");
    }

    #[test]
    fn parse_bare_model_uses_default() {
        let s = ModelSpec::parse("gpt-4o", "http://default/v1", false).expect("parse resolves");
        assert_eq!(s.provider, "default");
        assert_eq!(s.model, "gpt-4o");
        assert_eq!(s.base_url, "http://default/v1");
    }

    #[test]
    fn parse_unknown_provider_keeps_whole_spec() {
        let s = ModelSpec::parse("futureai/new-model", "http://default/v1", false)
            .expect("parse resolves");
        assert_eq!(s.provider, "default");
        assert_eq!(s.model, "futureai/new-model");
        assert_eq!(s.base_url, "http://default/v1");
    }

    #[test]
    fn pricing_math_for_chinese_models() {
        let p = model_pricing("deepseek", "deepseek-v4-flash").unwrap();
        assert_eq!(p, (0.14, 0.28));
        assert!((call_cost_usd(1_000_000, 1_000_000, p) - 0.42).abs() < 1e-9);

        let p = model_pricing("zai", "glm-5.2").unwrap();
        assert_eq!(p, (1.40, 4.40));

        let p = model_pricing("moonshot", "kimi-k2.7").unwrap();
        assert_eq!(p, (0.95, 4.00));
        let p = model_pricing("moonshot", "kimi-k3").unwrap();
        assert_eq!(p, (3.00, 15.00));
    }

    #[test]
    fn pricing_unknown_returns_none() {
        assert_eq!(model_pricing("zai", "glm-9.9"), None);
        assert_eq!(model_pricing("nope", "x"), None);

        assert_eq!(model_pricing("zai", "glm-4.6"), None);
    }

    #[test]
    fn featured_models_include_chinese_floors() {
        let f = featured_models();
        assert!(f.contains(&("zai".to_string(), "glm-5.2".to_string())));
        assert!(f.contains(&("moonshot".to_string(), "kimi-k2.7".to_string())));
        assert!(f.contains(&("deepseek".to_string(), "deepseek-v4-flash".to_string())));

        assert!(f.contains(&("ollama".to_string(), "qwen3-coder".to_string())));
        assert!(f.contains(&(
            "openrouter".to_string(),
            "qwen/qwen3-coder:free".to_string()
        )));
    }

    #[test]
    fn authable_providers_start_with_price_floors() {
        let keys: Vec<&str> = authable_providers().iter().map(|p| p.key).collect();
        assert_eq!(&keys[..3], &["deepseek", "zai", "moonshot"]);
        assert!(!keys.contains(&"ollama"));
        assert!(!keys.contains(&"gemini"));
    }

    #[test]
    fn cheap_first_order_puts_cloudflare_first() {
        let order = cheap_first_order();

        assert_eq!(order[0], "cloudflare");
        assert_eq!(order[1], "deepseek");

        let zi = order.iter().position(|k| k == "zai").unwrap();
        let mi = order.iter().position(|k| k == "moonshot").unwrap();
        assert!(mi < zi, "moonshot ($0.95) should sort before zai ($1.40)");
    }

    #[test]
    fn api_key_prefers_provider_env_then_ambient() {
        let s =
            ModelSpec::parse("zai/glm-5.2", "http://default/v1", false).expect("parse resolves");

        assert_eq!(
            s.api_key(Some("ambient-key")),
            Some("ambient-key".to_string())
        );
        assert_eq!(s.api_key(None), None);
    }

    /// RAII guard restoring one env var's previous value on drop
    /// (panic-safe): the manual restore after `f()` in
    /// `with_account_id_env` never ran when the closure panicked.
    /// Same shape as the EnvGuard in zen-cli's settings module,
    /// duplicated here because that guard is `pub(crate)` to zen-cli.
    #[must_use = "a guard dropped immediately restores the var at once; bind it for the whole scope"]
    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        /// Capture the current value of `key`, then set it to `value`.
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: callers hold the module lock while mutating env.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        /// Capture the current value of `key`, then remove it.
        fn remove(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: same as `set` above.
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }

        /// Set `key` to `value`, or remove it when `value` is `None`.
        fn set_or_remove(key: &'static str, value: Option<&str>) -> Self {
            match value {
                Some(v) => Self::set(key, v),
                None => Self::remove(key),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as `set` above.
            unsafe {
                match self.old.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn with_account_id_env(value: Option<&str>, f: impl FnOnce()) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The guard restores the previous value even if `f` panics;
        // it drops before the lock guard (reverse declaration order).
        let _env = EnvGuard::set_or_remove("CLOUDFLARE_ACCOUNT_ID", value);
        f();
    }

    #[test]
    fn cloudflare_entry_is_correct() {
        let p = find_provider("cloudflare").unwrap();
        assert_eq!(p.api_key_env, Some("CLOUDFLARE_API_TOKEN"));
        assert!(p.anthropic_base_url.is_none());

        let (std, url) = p.endpoint(false).unwrap();
        assert_eq!(std, ApiStandard::OpenAi);
        assert!(
            url.contains("{ACCOUNT_ID}"),
            "Workers AI embeds the account id in the URL path"
        );
    }

    #[test]
    fn cloudflare_template_resolves_with_account_id() {
        with_account_id_env(Some("test-acct-123"), || {
            let p = find_provider("cloudflare").unwrap();
            let (std, url) = p.endpoint_resolved(false).expect("must resolve");
            assert_eq!(std, ApiStandard::OpenAi);
            assert_eq!(
                url,
                "https://api.cloudflare.com/client/v4/accounts/test-acct-123/ai/v1"
            );

            let s = ModelSpec::parse(
                "cloudflare/@cf/meta/llama-3.1-8b-instruct",
                "http://default/v1",
                false,
            )
            .expect("cloudflare resolves with account id");
            assert_eq!(s.provider, "cloudflare");
            assert_eq!(s.model, "@cf/meta/llama-3.1-8b-instruct");
            assert_eq!(
                s.base_url,
                "https://api.cloudflare.com/client/v4/accounts/test-acct-123/ai/v1"
            );
            assert_eq!(s.api_key_env.as_deref(), Some("CLOUDFLARE_API_TOKEN"));
        });
    }

    #[test]
    fn cloudflare_without_account_id_is_an_error() {
        // Issue #346: a known provider whose endpoint cannot resolve
        // must not silently fall back to the default gateway - the
        // explicit provider choice, model and key env would be
        // discarded and the whole spec sent as a model name instead.
        with_account_id_env(None, || {
            let p = find_provider("cloudflare").unwrap();
            assert!(p.endpoint_resolved(false).is_none());
            let err = ModelSpec::parse("cloudflare/some-model", "http://default/v1", false)
                .expect_err("known provider that cannot resolve must error");
            assert!(
                err.contains("cloudflare") && err.contains("CLOUDFLARE_ACCOUNT_ID"),
                "error must name the provider and what it needs: {err}"
            );
        });
    }

    #[test]
    fn anthropic_without_anthropic_client_is_an_error() {
        // Issue #346: `anthropic/...` has no OpenAI-compatible endpoint,
        // so with no Anthropic client it cannot resolve - and must not
        // fall back to the default gateway either.
        let err = ModelSpec::parse("anthropic/claude-sonnet-4-6", "http://default/v1", false)
            .expect_err("anthropic without an Anthropic client must error");
        assert!(
            err.contains("anthropic") && err.contains("Anthropic"),
            "error must name the provider and what it needs: {err}"
        );

        let s = ModelSpec::parse("anthropic/claude-sonnet-4-6", "http://default/v1", true)
            .expect("anthropic resolves with an Anthropic client");
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.model, "claude-sonnet-4-6");
        assert_eq!(s.api_standard, ApiStandard::Anthropic);
        assert_eq!(s.base_url, "https://api.anthropic.com");
        assert_eq!(s.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn resolve_url_template_passes_plain_urls_through() {
        assert_eq!(
            resolve_url_template("https://api.deepseek.com/v1"),
            Some("https://api.deepseek.com/v1".to_string())
        );

        with_account_id_env(Some(""), || {
            assert_eq!(resolve_url_template("https://x/{ACCOUNT_ID}/y"), None);
        });
    }

    #[test]
    fn cloudflare_clef_flash_pricing_is_verified() {
        let p = model_pricing("cloudflare", "@cf/cloudflare/clef-flash").unwrap();
        assert_eq!(p, (0.09, 0.0));

        assert_eq!(
            model_pricing("cloudflare", "@cf/meta/llama-3.1-8b-instruct"),
            None
        );
    }

    #[test]
    fn featured_models_lead_with_cloudflare_free_tier() {
        let f = featured_models();
        assert!(f.contains(&(
            "cloudflare".to_string(),
            "@cf/cloudflare/clef-flash".to_string()
        )));
        assert!(f.contains(&(
            "cloudflare".to_string(),
            "@cf/meta/llama-3.1-8b-instruct".to_string()
        )));
    }

    #[test]
    fn authable_providers_include_cloudflare_after_moonshot() {
        let keys: Vec<&str> = authable_providers().iter().map(|p| p.key).collect();
        assert_eq!(&keys[..4], &["deepseek", "zai", "moonshot", "cloudflare"]);
    }
}
