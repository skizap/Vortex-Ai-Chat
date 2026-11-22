//! Verified open-weights mapping and the recommended-shortlist computation
//! (plan step 3). Everything here is a pure, deterministic function of the
//! provider catalog plus a small reviewed exact-ID table, so it is testable
//! offline with fixtures.
//!
//! Honesty rules encoded here:
//! - A Hugging Face id, a brand name or a free price alone is **never** proof
//!   of open weights; only an entry in [`VERIFIED_OPEN_MODELS`] sets
//!   `open_weights`, and every entry was reviewed against the publisher's
//!   license text (review date recorded per table revision).
//! - Weekly rank means *weekly token usage* (provider `sort=top-weekly`), not
//!   quality. When ranks are unavailable, popularity claims must be omitted.
//! - Unknown prices are never zero: a model without both parseable token
//!   rates stays out of Recommended (but remains visible in the full catalog).
//! - Routers, aliases and non-interactive (`:batch`) endpoints are excluded.
//! - Custom license restrictions are flagged and keep the model out of the
//!   default shortlist.
//! - A mapped model the catalog no longer lists is never advertised.

use vortex_types::{ModelInfo, RecommendedBounds};

/// Provenance string reported alongside weekly ranks.
pub const RANKING_SOURCE: &str = "openrouter sort=top-weekly (weekly token usage)";
/// Shortlist size when the client does not edit it.
pub const DEFAULT_RECOMMENDED_LIMIT: u32 = 5;
/// Server-side ceiling for the editable shortlist size.
pub const MAX_RECOMMENDED_LIMIT: u32 = 25;

/// One reviewed open-weights model, keyed by its **exact** provider id.
/// Variant mappings (e.g. an OpenRouter id whose weights live under a
/// differently named publisher repo) were checked individually; never assume
/// an entire family qualifies.
pub struct VerifiedOpenModel {
    /// Exact catalog id (case-sensitive).
    pub id: &'static str,
    /// Publisher of the weights.
    pub publisher: &'static str,
    /// License name as published by the publisher.
    pub license: &'static str,
    /// Link to the license text (or the model card carrying it).
    pub license_url: &'static str,
    /// Link to the publisher's weights page.
    pub weights_url: &'static str,
    /// Custom terms beyond the standard license text. `Some` keeps the model
    /// out of the default shortlist; it stays visible (and flagged) in the
    /// full catalog.
    pub restriction: Option<&'static str>,
}

const MIT_URL: &str = "https://opensource.org/licenses/MIT";
const APACHE_URL: &str = "https://www.apache.org/licenses/LICENSE-2.0";

/// Reviewed exact-ID mapping. Licenses verified against publisher sources on
/// 2026-11-22 (HF `cardData.license` plus the repository LICENSE text):
/// DeepSeek ships plain MIT; Qwen3.x ships Apache-2.0 — but Qwen2.5-72B
/// Instruct ships the custom Qwen LICENSE AGREEMENT, proving family names
/// carry no blanket guarantee. Mistral Small/NeMo/Ministral ship Apache-2.0,
/// while Mistral Large 4 and Devstral 2 report "other" and are deliberately
/// **not** mapped. Entries must be removed when a model retires.
pub const VERIFIED_OPEN_MODELS: &[VerifiedOpenModel] = &[
    // DeepSeek — MIT (LICENSE reviewed: plain MIT, no attribution rider).
    VerifiedOpenModel {
        id: "deepseek/deepseek-v4.1-flash-1017",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash-1017",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "deepseek/deepseek-v4.1-flash",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "deepseek/deepseek-v4-flash",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-V4-Flash",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "deepseek/deepseek-v3.2",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-V3.2",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "deepseek/deepseek-chat-v3.1",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-V3.1",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "deepseek/deepseek-r1-0528",
        publisher: "DeepSeek",
        license: "MIT",
        license_url: MIT_URL,
        weights_url: "https://huggingface.co/deepseek-ai/DeepSeek-R1-0528",
        restriction: None,
    },
    // Qwen — Apache-2.0 per model card (smaller Qwen2.5 variants are Apache
    // too, but the 72B Instruct is not: see the restricted entry below).
    VerifiedOpenModel {
        id: "qwen/qwen3.8-27b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3.8-27B",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3.6-35b-a3b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3.6-35B-A3B",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3.5-9b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3.5-9B",
        restriction: None,
    },
    // Variant mapping verified: this id serves the Instruct-2507 weights.
    VerifiedOpenModel {
        id: "qwen/qwen3-235b-a22b-2507",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-235B-A22B-Instruct-2507",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3.5-35b-a3b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3.5-35B-A3B",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3-30b-a3b-instruct-2507",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-30B-A3B-Instruct-2507",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3.5-397b-a17b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3.5-397B-A17B",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3-32b",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-32B",
        restriction: None,
    },
    // Variant mapping verified: serves Qwen3-Coder-480B-A35B-Instruct.
    VerifiedOpenModel {
        id: "qwen/qwen3-coder",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-Coder-480B-A35B-Instruct",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3-coder-30b-a3b-instruct",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-Coder-30B-A3B-Instruct",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "qwen/qwen3-235b-a22b-thinking-2507",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/Qwen/Qwen3-235B-A22B-Thinking-2507",
        restriction: None,
    },
    // Open weights under a custom agreement: flagged, never default-recommended.
    VerifiedOpenModel {
        id: "qwen/qwen-2.5-72b-instruct",
        publisher: "Alibaba Cloud Qwen Team",
        license: "Qwen License",
        license_url: "https://huggingface.co/Qwen/Qwen2.5-72B-Instruct/blob/main/LICENSE",
        weights_url: "https://huggingface.co/Qwen/Qwen2.5-72B-Instruct",
        restriction: Some(
            "Qwen LICENSE AGREEMENT: custom (non-OSI) redistribution and attribution terms; \
             reviewed 2026-11-22. Open weights, but not recommended by default.",
        ),
    },
    // Mistral — Apache-2.0 per model cards. Mistral Large 4 and Devstral 2
    // report "other" and are intentionally absent from this table.
    VerifiedOpenModel {
        id: "mistralai/mistral-nemo",
        publisher: "Mistral AI",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/mistralai/Mistral-Nemo-Instruct-2407",
        restriction: None,
    },
    // Variant mapping verified: serves Mistral-Small-3.2-24B-Instruct-2506.
    VerifiedOpenModel {
        id: "mistralai/mistral-small-3.2-24b-instruct",
        publisher: "Mistral AI",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/mistralai/Mistral-Small-3.2-24B-Instruct-2506",
        restriction: None,
    },
    // Variant mapping verified: id says "small-2603", catalog name is
    // "Mistral Small 4", weights live at Mistral-Small-4-119B-2603.
    VerifiedOpenModel {
        id: "mistralai/mistral-small-2603",
        publisher: "Mistral AI",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/mistralai/Mistral-Small-4-119B-2603",
        restriction: None,
    },
    VerifiedOpenModel {
        id: "mistralai/mistral-small-24b-instruct-2501",
        publisher: "Mistral AI",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/mistralai/Mistral-Small-24B-Instruct-2501",
        restriction: None,
    },
    // Variant mapping verified: serves Ministral-3-8B-Instruct-2512.
    VerifiedOpenModel {
        id: "mistralai/ministral-8b-2512",
        publisher: "Mistral AI",
        license: "Apache-2.0",
        license_url: APACHE_URL,
        weights_url: "https://huggingface.co/mistralai/Ministral-3-8B-Instruct-2512",
        restriction: None,
    },
];

/// Exact-id lookup into the reviewed table.
pub fn verified_by_id(id: &str) -> Option<&'static VerifiedOpenModel> {
    VERIFIED_OPEN_MODELS.iter().find(|v| v.id == id)
}

/// Attach reviewed license/publisher metadata to catalog entries. Unmapped
/// models are returned unchanged — `open_weights` stays `None` for them, so
/// clients can distinguish "verified open weights" from "unknown lineage".
pub fn enrich(models: &[ModelInfo]) -> Vec<ModelInfo> {
    models
        .iter()
        .map(|m| {
            let mut out = m.clone();
            if let Some(v) = verified_by_id(&m.id) {
                out.open_weights = Some(true);
                out.publisher = Some(v.publisher.to_string());
                out.license = Some(v.license.to_string());
                out.license_url = Some(v.license_url.to_string());
                out.weights_url = Some(v.weights_url.to_string());
                out.restriction = v.restriction.map(str::to_string);
            }
            out
        })
        .collect()
}

/// Ranking provenance: `Some` only when the catalog actually carries weekly
/// ranks; clients must omit popularity claims when this is `None`.
pub fn ranking_source(models: &[ModelInfo]) -> Option<&'static str> {
    models
        .iter()
        .any(|m| m.weekly_rank.is_some())
        .then_some(RANKING_SOURCE)
}

/// Routers, alias targets and non-interactive endpoints are never recommended.
pub fn is_router_or_batch(id: &str) -> bool {
    id.starts_with("openrouter/")
        || id.starts_with('~')
        || id.ends_with("/auto")
        || id.ends_with(":batch")
}

/// Parse a provider USD-per-token rate into USD per million tokens. Missing,
/// unparseable, negative or non-finite rates stay unknown — never zero.
fn rate_per_mtok(rate: &Option<String>) -> Option<f64> {
    let value = rate.as_deref()?.trim().parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value * 1_000_000.0)
}

/// Float slack so a published rate that *equals* a bound (e.g. $3/M output
/// parsed as 0.000003) is treated as within bounds despite binary rounding.
const PRICE_EPSILON: f64 = 1e-9;

/// Clamp the editable shortlist size into its server-side range.
pub fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_RECOMMENDED_LIMIT)
}

/// The recommended shortlist, at an explicit "today" (YYYY-MM-DD) so tests are
/// deterministic. Order: weekly rank ascending (weekly token usage, not
/// quality); unranked entries last, ties by id. Excluded: unmapped ids,
/// routers/aliases/batch endpoints, custom-restriction licenses, models past
/// their provider-reported expiration date, and models whose input **or**
/// output price is unknown, unparseable, or above its independent bound
/// (published zero rates are within bounds).
pub fn recommended_on(
    models: &[ModelInfo],
    bounds: RecommendedBounds,
    limit: u32,
    today: &str,
) -> Vec<String> {
    let mut picks: Vec<(Option<u32>, &str)> = models
        .iter()
        .filter_map(|m| {
            if is_router_or_batch(&m.id) {
                return None;
            }
            let verified = verified_by_id(&m.id)?;
            if verified.restriction.is_some() {
                return None;
            }
            // Retired (or retiring today) models are never advertised, even
            // if the provider still lists them.
            if let Some(expires) = m.expires_on.as_deref() {
                if !expires.is_empty() && expires <= today {
                    return None;
                }
            }
            let input = rate_per_mtok(&m.pricing_prompt)?;
            let output = rate_per_mtok(&m.pricing_completion)?;
            if input > bounds.max_input_usd_per_mtok + PRICE_EPSILON {
                return None;
            }
            if output > bounds.max_output_usd_per_mtok + PRICE_EPSILON {
                return None;
            }
            Some((m.weekly_rank, m.id.as_str()))
        })
        .collect();
    // Ranked models lead (weekly rank ascending); unranked follow by id.
    // Note `None < Some(_)` for Option, so the sort key maps None past every
    // real rank instead of relying on Option's own ordering.
    picks.sort_by(|a, b| {
        a.0.unwrap_or(u32::MAX)
            .cmp(&b.0.unwrap_or(u32::MAX))
            .then_with(|| a.1.cmp(b.1))
    });
    picks
        .into_iter()
        .take(clamp_limit(limit) as usize)
        .map(|(_, id)| id.to_string())
        .collect()
}

/// [`recommended_on`] with the current UTC date.
pub fn recommended(models: &[ModelInfo], bounds: RecommendedBounds, limit: u32) -> Vec<String> {
    let today = chrono::Utc::now().date_naive().to_string();
    recommended_on(models, bounds, limit, &today)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, rank: Option<u32>, input: Option<&str>, output: Option<&str>) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            weekly_rank: rank,
            pricing_prompt: input.map(str::to_string),
            pricing_completion: output.map(str::to_string),
            context_length: Some(128_000),
            input_modalities: vec!["text".into()],
            output_modalities: vec!["text".into()],
            ..Default::default()
        }
    }

    const TODAY: &str = "2026-11-22";

    /// Fixture: weekly order versus price order — the shortlist follows the
    /// documented weekly token-usage ranking, never the cheapest-first order.
    #[test]
    fn weekly_rank_order_beats_price_order() {
        let catalog = [
            // Cheapest, but weekly rank 3.
            model(
                "qwen/qwen3-32b",
                Some(3),
                Some("0.00000008"),
                Some("0.00000028"),
            ),
            // Pricier, weekly rank 1.
            model(
                "deepseek/deepseek-chat-v3.1",
                Some(1),
                Some("0.00000025"),
                Some("0.00000095"),
            ),
            // Mid price, weekly rank 2.
            model(
                "mistralai/mistral-nemo",
                Some(2),
                Some("0.000000029"),
                Some("0.00000003"),
            ),
        ];
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec![
                "deepseek/deepseek-chat-v3.1",
                "mistralai/mistral-nemo",
                "qwen/qwen3-32b",
            ]
        );
    }

    /// Fixture: input and output bounds are independent — exceeding either one
    /// alone excludes; published zero rates and exact-boundary rates qualify.
    #[test]
    fn price_bounds_exceeded_independently() {
        let catalog = [
            // Input $2/M > $1 bound (output fine).
            model(
                "qwen/qwen3.5-9b",
                Some(1),
                Some("0.000002"),
                Some("0.000001"),
            ),
            // Output $5/M > $3 bound (input fine).
            model(
                "qwen/qwen3-32b",
                Some(2),
                Some("0.0000001"),
                Some("0.000005"),
            ),
            // Exactly at both bounds: $1/M in, $3/M out — within.
            model(
                "qwen/qwen3.5-397b-a17b",
                Some(3),
                Some("0.000001"),
                Some("0.000003"),
            ),
            // Published zero rates: within.
            model("mistralai/mistral-nemo", Some(4), Some("0"), Some("0")),
        ];
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec!["qwen/qwen3.5-397b-a17b", "mistralai/mistral-nemo"]
        );
    }

    /// Fixture: free versus unknown price — an explicit "0" qualifies; a
    /// missing, unparseable or negative rate never counts as zero and keeps
    /// the model out of Recommended while it stays in the enriched catalog.
    #[test]
    fn free_price_qualifies_unknown_price_never_does() {
        let catalog = [
            model("qwen/qwen3-32b", Some(1), Some("0"), Some("0")),
            model("qwen/qwen3.5-9b", Some(2), None, Some("0")),
            model("qwen/qwen3.6-35b-a3b", Some(3), Some("n/a"), Some("0")),
            model("qwen/qwen3-coder", Some(4), Some("-1"), Some("0")),
        ];
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec!["qwen/qwen3-32b"]
        );
        // Still listed (and unmapped-metadata-free) in the enriched catalog.
        let enriched = enrich(&catalog);
        assert_eq!(enriched.len(), 4);
        assert_eq!(enriched[1].pricing_prompt, None);
    }

    /// Fixture: restricted license — open weights under custom terms are
    /// flagged by enrichment and excluded from the default shortlist.
    #[test]
    fn restricted_license_is_flagged_and_excluded() {
        let catalog = [
            model(
                "qwen/qwen-2.5-72b-instruct",
                Some(1),
                Some("0.00000036"),
                Some("0.0000004"),
            ),
            model(
                "qwen/qwen3-32b",
                Some(2),
                Some("0.00000008"),
                Some("0.00000028"),
            ),
        ];
        let enriched = enrich(&catalog);
        assert_eq!(enriched[0].open_weights, Some(true));
        assert_eq!(enriched[0].license.as_deref(), Some("Qwen License"));
        assert!(enriched[0].restriction.is_some());
        assert_eq!(enriched[1].license.as_deref(), Some("Apache-2.0"));
        assert!(enriched[1].restriction.is_none());
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec!["qwen/qwen3-32b"]
        );
    }

    /// Fixture: missing/retired model — a mapped id the catalog no longer
    /// lists is simply absent (never advertised), and a model at or past its
    /// provider-reported expiration date drops out while a future date keeps
    /// it (the date itself travels to the UI via `expires_on`).
    #[test]
    fn missing_and_retired_models_are_never_advertised() {
        // deepseek/deepseek-v4.1-flash is mapped but absent from this catalog.
        let mut expiring = model(
            "mistralai/mistral-small-3.2-24b-instruct",
            Some(1),
            Some("0.000000075"),
            Some("0.0000002"),
        );
        expiring.expires_on = Some(TODAY.to_string()); // retires today → out
        let mut future = model(
            "qwen/qwen3-32b",
            Some(2),
            Some("0.00000008"),
            Some("0.00000028"),
        );
        future.expires_on = Some("2027-06-01".to_string());
        let catalog = [expiring.clone(), future.clone()];
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec!["qwen/qwen3-32b"]
        );
        // Before the expiration date it is recommended, with the date visible.
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, "2026-11-21"),
            vec!["mistralai/mistral-small-3.2-24b-instruct", "qwen/qwen3-32b"]
        );
        assert_eq!(
            enrich(&catalog)[1].expires_on.as_deref(),
            Some("2027-06-01")
        );
    }

    /// Fixture: stale/unavailable ranking — with no weekly ranks anywhere the
    /// shortlist still computes (ordered by id) but `ranking_source` reports
    /// unavailable so clients omit popularity claims.
    #[test]
    fn unavailable_ranking_omits_popularity_claims() {
        let catalog = [
            model("qwen/qwen3-32b", None, Some("0"), Some("0")),
            model("mistralai/mistral-nemo", None, Some("0"), Some("0")),
        ];
        assert_eq!(ranking_source(&catalog), None);
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 5, TODAY),
            vec!["mistralai/mistral-nemo", "qwen/qwen3-32b"]
        );
        // Ranked entries lead unranked ones.
        let mixed = [
            model("qwen/qwen3-32b", None, Some("0"), Some("0")),
            model("mistralai/mistral-nemo", Some(9), Some("0"), Some("0")),
        ];
        assert_eq!(ranking_source(&mixed), Some(RANKING_SOURCE));
        assert_eq!(
            recommended_on(&mixed, RecommendedBounds::default(), 5, TODAY),
            vec!["mistralai/mistral-nemo", "qwen/qwen3-32b"]
        );
    }

    /// Fixture: routers and non-interactive endpoints — aliases, tilde-latest
    /// pointers and `:batch` listings never enter the shortlist, and a router
    /// without a reported response limit keeps `max_completion_tokens: None`
    /// (never invented).
    #[test]
    fn routers_aliases_and_batch_endpoints_are_excluded() {
        assert!(is_router_or_batch("openrouter/auto"));
        assert!(is_router_or_batch("~deepseek/deepseek-flash-latest"));
        assert!(is_router_or_batch("deepseek/deepseek-v4.1-flash:batch"));
        assert!(is_router_or_batch("vendor/auto"));
        assert!(!is_router_or_batch("deepseek/deepseek-v4.1-flash"));

        let mut router = model("openrouter/auto", Some(1), Some("0"), Some("0"));
        router.max_completion_tokens = None;
        // A mapped id wearing a batch suffix is a different, excluded id.
        let batch = model("qwen/qwen3-32b:batch", Some(2), Some("0"), Some("0"));
        let real = model("qwen/qwen3-32b", Some(3), Some("0"), Some("0"));
        assert_eq!(
            recommended_on(
                &[router, batch, real],
                RecommendedBounds::default(),
                5,
                TODAY
            ),
            vec!["qwen/qwen3-32b"]
        );
    }

    /// Editable bounds and limit flow through: a free-only bound keeps just
    /// zero-rated models, and the limit truncates after ranking.
    #[test]
    fn edited_bounds_and_limit_apply() {
        let catalog = [
            model(
                "deepseek/deepseek-chat-v3.1",
                Some(1),
                Some("0.00000025"),
                Some("0.00000095"),
            ),
            model("qwen/qwen3-32b", Some(2), Some("0"), Some("0")),
            model("mistralai/mistral-nemo", Some(3), Some("0"), Some("0")),
        ];
        let free_only = RecommendedBounds::clamped(0.0, 0.0);
        assert_eq!(
            recommended_on(&catalog, free_only, 5, TODAY),
            vec!["qwen/qwen3-32b", "mistralai/mistral-nemo"]
        );
        assert_eq!(
            recommended_on(&catalog, RecommendedBounds::default(), 1, TODAY),
            vec!["deepseek/deepseek-chat-v3.1"]
        );
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(1000), MAX_RECOMMENDED_LIMIT);
        assert_eq!(clamp_limit(DEFAULT_RECOMMENDED_LIMIT), 5);
        // NaN/negative bounds fall back to defaults per field.
        assert_eq!(
            RecommendedBounds::clamped(f64::NAN, -1.0),
            RecommendedBounds::default()
        );
    }

    /// Enrichment never guesses: unmapped models keep `open_weights: None`
    /// even with a Hugging-Face-looking id or a free price, and mapped models
    /// gain the full reviewed provenance.
    #[test]
    fn enrichment_comes_only_from_the_reviewed_table() {
        let unmapped = ModelInfo {
            id: "vendor/mystery".into(),
            pricing_prompt: Some("0".into()),
            pricing_completion: Some("0".into()),
            ..Default::default()
        };
        let mapped = model(
            "deepseek/deepseek-v4.1-flash",
            Some(2),
            Some("0.0000000498"),
            Some("0.0000001992"),
        );
        let enriched = enrich(&[unmapped, mapped]);
        assert!(enriched[0].open_weights.is_none());
        assert!(enriched[0].license.is_none());
        assert!(enriched[0].publisher.is_none());
        assert_eq!(enriched[1].open_weights, Some(true));
        assert_eq!(enriched[1].publisher.as_deref(), Some("DeepSeek"));
        assert_eq!(enriched[1].license.as_deref(), Some("MIT"));
        assert_eq!(
            enriched[1].weights_url.as_deref(),
            Some("https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash")
        );
        assert_eq!(enriched[1].weekly_rank, Some(2), "rank passes through");
    }

    /// The reviewed table itself stays sane: unique exact ids, no router or
    /// batch entries, every entry fully attributed, and restriction ⇒ flagged.
    #[test]
    fn verified_table_is_consistent() {
        let mut ids: Vec<&str> = VERIFIED_OPEN_MODELS.iter().map(|v| v.id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate ids in VERIFIED_OPEN_MODELS");
        for v in VERIFIED_OPEN_MODELS {
            assert!(
                !is_router_or_batch(v.id),
                "router/batch id mapped: {}",
                v.id
            );
            assert!(!v.publisher.is_empty() && !v.license.is_empty());
            assert!(v.license_url.starts_with("https://"));
            assert!(v.weights_url.starts_with("https://"));
            if v.restriction.is_some() {
                assert_ne!(v.license, "Apache-2.0");
                assert_ne!(v.license, "MIT");
            }
        }
        // The families the plan named are represented.
        assert!(verified_by_id("qwen/qwen3-32b").is_some());
        assert!(verified_by_id("deepseek/deepseek-chat-v3.1").is_some());
        assert!(verified_by_id("mistralai/mistral-nemo").is_some());
        // Non-qualifying family members are absent.
        assert!(verified_by_id("mistralai/mistral-large-4-0").is_none());
        assert!(verified_by_id("mistralai/devstral-2512").is_none());
        assert!(verified_by_id("qwen/qwen4-max").is_none());
    }
}
