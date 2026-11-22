//! Server-side Vortex Auto model resolution (plan step 4).
//!
//! One resolution per turn, before the run is accepted: gather what the turn
//! actually needs (input modalities of the current attachments, tool schemas
//! that will really be sent, estimated context and the output allowance),
//! then pick the cheapest model that *affirmatively* matches with *usable*
//! prices. Everything here is a pure, deterministic function of the provider
//! catalog so it can be tested offline.
//!
//! Catalog metadata is display/ranking data, never instructions. Cost figures
//! are comparative estimates for ranking — not a bill.

use vortex_types::ModelInfo;

/// Coarse text token estimate (~4 chars/token). The result only needs to be
/// comparable across candidate models, not exact.
pub const CHARS_PER_TOKEN: u64 = 4;
/// Flat input-token estimates for visual media: providers meter these per
/// tile/frame and the catalog does not report it, so fixed constants keep the
/// comparison deterministic and honest about being an estimate.
pub const IMAGE_INPUT_TOKENS: u64 = 1_500;
pub const VIDEO_INPUT_TOKENS: u64 = 10_000;
/// Fallbacks when an attachment's size is unknown at estimate time.
pub const PDF_FALLBACK_TOKENS: u64 = 8_000;
pub const TEXT_FALLBACK_TOKENS: u64 = 500;

pub fn estimate_text_tokens(chars: u64) -> u64 {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// What an attachment will materialize into inside the model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentShape {
    Image,
    Video,
    Pdf,
    Text,
    /// Not sent to the model at all (unsupported type; placeholder only).
    Skipped,
}

/// Estimated input tokens one attachment contributes. PDFs are parsed to text
/// by the provider file-parser, so their raw size drives the estimate; the
/// same cap as hydration applies to text files.
pub fn attachment_tokens(shape: AttachmentShape, size_bytes: Option<u64>) -> u64 {
    match shape {
        AttachmentShape::Image => IMAGE_INPUT_TOKENS,
        AttachmentShape::Video => VIDEO_INPUT_TOKENS,
        AttachmentShape::Pdf => match size_bytes {
            Some(size) => estimate_text_tokens(size).max(1),
            None => PDF_FALLBACK_TOKENS,
        },
        AttachmentShape::Text => match size_bytes {
            Some(size) => {
                estimate_text_tokens(size.min(crate::media_context::TEXT_LIMIT as u64)).max(1)
            }
            None => TEXT_FALLBACK_TOKENS,
        },
        AttachmentShape::Skipped => 0,
    }
}

/// Everything the current turn requires from a candidate model.
#[derive(Debug, Clone)]
pub struct AutoRequirements {
    pub needs_image: bool,
    pub needs_video: bool,
    /// True when tool schemas will actually be sent with the request.
    pub needs_tools: bool,
    /// Number of image inputs expected in the request (drives known
    /// per-image fees).
    pub image_count: u32,
    /// Estimated prompt tokens for this turn's provider calls.
    pub est_input_tokens: u64,
    /// Reserved completion tokens (the per-call `max_tokens` allowance).
    pub output_allowance: u64,
}

/// Router aliases and non-chat listings Auto must never pick: the
/// `openrouter/` namespace, any generic `/auto` alias, and `:batch` variants
/// (batch-only endpoints cannot serve streaming chat completions). Concrete
/// variants such as `:free` stay eligible — a zero-priced, affirmatively
/// compatible model is exactly what cost-first ranking should surface.
pub fn is_router_alias(id: &str) -> bool {
    id.starts_with("openrouter/") || id.ends_with("/auto") || id.ends_with(":batch")
}

/// Parse a provider USD rate string. Missing, unparseable, negative or
/// non-finite rates stay unknown — never zero.
fn usable_rate(rate: &Option<String>) -> Option<f64> {
    let value = rate.as_deref()?.trim().parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Known applicable ancillary fees count toward the estimate; unknown ones
/// are ignored (only the token rates are mandatory).
fn known_fee(rate: &Option<String>) -> f64 {
    usable_rate(rate).unwrap_or(0.0)
}

/// Affirmative compatibility, known context headroom and usable token prices.
/// Unknown capabilities never qualify: Auto only picks models whose catalog
/// entry positively states everything the turn needs.
pub fn is_eligible(model: &ModelInfo, req: &AutoRequirements) -> bool {
    if is_router_alias(&model.id) {
        return false;
    }
    let input = |kind: &str| model.input_modalities.iter().any(|m| m == kind);
    if !input("text") {
        return false;
    }
    if !model.output_modalities.iter().any(|m| m == "text") {
        return false;
    }
    if req.needs_image && !input("image") {
        return false;
    }
    if req.needs_video && !input("video") {
        return false;
    }
    if req.needs_tools && model.supports_tools != Some(true) {
        return false;
    }
    let Some(context) = model.context_length else {
        return false;
    };
    if context < req.est_input_tokens.saturating_add(req.output_allowance) {
        return false;
    }
    // Missing rates are never treated as zero: both token rates must parse.
    usable_rate(&model.pricing_prompt).is_some() && usable_rate(&model.pricing_completion).is_some()
}

/// Comparative per-turn USD estimate used for ranking (not a bill):
/// estimated input tokens × prompt rate + output allowance × completion rate,
/// plus known applicable per-image and per-request fees.
pub fn estimated_cost_usd(model: &ModelInfo, req: &AutoRequirements) -> Option<f64> {
    let prompt = usable_rate(&model.pricing_prompt)?;
    let completion = usable_rate(&model.pricing_completion)?;
    let mut cost = req.est_input_tokens as f64 * prompt;
    cost += req.output_allowance as f64 * completion;
    cost += f64::from(req.image_count) * known_fee(&model.pricing_image);
    cost += known_fee(&model.pricing_request);
    Some(cost)
}

/// The deterministic pick: cheapest eligible model, ties broken by
/// lexicographic id so identical catalog + requirements always route
/// identically. `None` when nothing verifiably matches — callers must surface
/// refresh-or-pin guidance and never silently fall back to a paid model.
pub fn resolve_auto(catalog: &[ModelInfo], req: &AutoRequirements) -> Option<String> {
    catalog
        .iter()
        .filter(|m| is_eligible(m, req))
        .filter_map(|m| Some((m, estimated_cost_usd(m, req)?)))
        .min_by(|(a, cost_a), (b, cost_b)| cost_a.total_cmp(cost_b).then_with(|| a.id.cmp(&b.id)))
        .map(|(m, _)| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> AutoRequirements {
        AutoRequirements {
            needs_image: false,
            needs_video: false,
            needs_tools: true,
            image_count: 0,
            est_input_tokens: 1_000,
            output_allowance: 500,
        }
    }

    /// An affirmatively compatible, priced text+image model.
    fn model(id: &str, prompt: &str, completion: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context_length: Some(128_000),
            supports_tools: Some(true),
            pricing_prompt: Some(prompt.into()),
            pricing_completion: Some(completion.into()),
            created: Some(1_700_000_000),
            pricing_request: None,
            pricing_image: None,
            input_modalities: vec!["text".into(), "image".into()],
            output_modalities: vec!["text".into()],
            ..ModelInfo::default()
        }
    }

    #[test]
    fn picks_the_cheapest_eligible_model() {
        let catalog = [
            model("vendor/mid", "0.000001", "0.000003"),
            model("vendor/cheap", "0.0000001", "0.0000003"),
            model("vendor/pricey", "0.00001", "0.00003"),
        ];
        assert_eq!(
            resolve_auto(&catalog, &req()).as_deref(),
            Some("vendor/cheap")
        );
    }

    #[test]
    fn output_allowance_participates_in_ranking() {
        // A is cheaper per prompt token, B per completion token: a large
        // output allowance must flip the ranking toward B.
        let a = model("vendor/a", "0.0000001", "0.00001");
        let b = model("vendor/b", "0.0000005", "0.0000002");
        let mut small = req();
        small.output_allowance = 10;
        assert_eq!(
            resolve_auto(&[a.clone(), b.clone()], &small).as_deref(),
            Some("vendor/a")
        );
        let mut large = req();
        large.output_allowance = 100_000;
        assert_eq!(resolve_auto(&[a, b], &large).as_deref(), Some("vendor/b"));
    }

    #[test]
    fn ties_break_deterministically_by_id() {
        let catalog = [
            model("vendor/zzz", "0.000001", "0.000002"),
            model("vendor/aaa", "0.000001", "0.000002"),
            model("vendor/mmm", "0.000001", "0.000002"),
        ];
        assert_eq!(
            resolve_auto(&catalog, &req()).as_deref(),
            Some("vendor/aaa")
        );
        // Catalog order does not matter.
        let shuffled = [catalog[2].clone(), catalog[0].clone(), catalog[1].clone()];
        assert_eq!(
            resolve_auto(&shuffled, &req()).as_deref(),
            Some("vendor/aaa")
        );
    }

    #[test]
    fn missing_rates_are_never_zero() {
        // The "free-looking" model has no published prompt rate: it must not
        // win by treating the unknown as zero.
        let mut unknown = model("vendor/unknown", "0", "0");
        unknown.pricing_prompt = None;
        let paid = model("vendor/paid", "0.00001", "0.00003");
        assert_eq!(
            resolve_auto(&[unknown.clone(), paid.clone()], &req()).as_deref(),
            Some("vendor/paid")
        );
        // Unparseable and negative rates are equally unusable.
        let mut bad = model("vendor/bad", "0", "0");
        bad.pricing_completion = Some("not-a-number".into());
        assert!(!is_eligible(&bad, &req()));
        let mut negative = model("vendor/neg", "-1", "0");
        assert!(!is_eligible(&negative, &req()));
        negative.pricing_prompt = Some("nan".into());
        assert!(!is_eligible(&negative, &req()));
        // Explicit zero is a usable price (a genuinely free model).
        assert_eq!(
            resolve_auto(&[model("vendor/free", "0", "0"), paid], &req()).as_deref(),
            Some("vendor/free")
        );
        assert!(!is_eligible(&unknown, &req()));
    }

    #[test]
    fn free_variants_stay_eligible_but_router_and_batch_ids_do_not() {
        assert!(is_router_alias("openrouter/auto"));
        assert!(is_router_alias("openrouter/whatever"));
        assert!(is_router_alias("vendor/auto"));
        assert!(is_router_alias("google/gemini-x:batch"));
        assert!(!is_router_alias("meta-llama/llama-3.3-70b-instruct:free"));
        assert!(!is_router_alias("vendor/model:online"));
        assert!(!is_router_alias("vendor/model"));

        let mut batch = model("vendor/cheapest:batch", "0", "0");
        batch.input_modalities = vec!["text".into(), "image".into(), "video".into()];
        let mut alias = model("openrouter/auto", "0", "0");
        alias.input_modalities = vec!["text".into(), "image".into(), "video".into()];
        let free = model("vendor/cheapest:free", "0", "0");
        let mut r = req();
        r.needs_video = true;
        assert_eq!(
            resolve_auto(&[batch, alias, free.clone()], &r).as_deref(),
            None,
            ":free variant lacks video input; aliases are excluded outright"
        );
        r.needs_video = false;
        assert_eq!(
            resolve_auto(&[free], &r).as_deref(),
            Some("vendor/cheapest:free")
        );
    }

    #[test]
    fn unknown_capabilities_are_not_affirmative() {
        let mut silent_inputs = model("vendor/silent", "0.000001", "0.000002");
        silent_inputs.input_modalities = Vec::new();
        assert!(!is_eligible(&silent_inputs, &req()));
        let mut no_text_out = model("vendor/image-out", "0.000001", "0.000002");
        no_text_out.output_modalities = vec!["image".into()];
        assert!(!is_eligible(&no_text_out, &req()));
        let mut unknown_ctx = model("vendor/no-ctx", "0.000001", "0.000002");
        unknown_ctx.context_length = None;
        assert!(!is_eligible(&unknown_ctx, &req()));
    }

    #[test]
    fn media_requirements_gate_eligibility() {
        let mut text_only = model("vendor/text", "0", "0");
        text_only.input_modalities = vec!["text".into()];
        let image_only = model("vendor/image", "0", "0");
        let mut video_capable = image_only.clone();
        video_capable.id = "vendor/video".into();
        video_capable.input_modalities = vec!["text".into(), "image".into(), "video".into()];

        let mut r = req();
        r.needs_image = true;
        r.image_count = 1;
        assert_eq!(
            resolve_auto(&[text_only.clone(), image_only.clone()], &r).as_deref(),
            Some("vendor/image")
        );
        r.needs_video = true;
        assert_eq!(
            resolve_auto(
                &[text_only.clone(), image_only.clone(), video_capable.clone()],
                &r
            )
            .as_deref(),
            Some("vendor/video")
        );
        assert_eq!(resolve_auto(&[text_only, image_only], &r), None);
    }

    #[test]
    fn tools_must_be_affirmatively_supported_when_sent() {
        let mut unknown_tools = model("vendor/unknown-tools", "0", "0");
        unknown_tools.supports_tools = None;
        let mut no_tools = model("vendor/no-tools", "0", "0");
        no_tools.supports_tools = Some(false);
        let with_tools = model("vendor/with-tools", "0.00001", "0.00001");

        let mut r = req();
        r.needs_tools = true;
        assert_eq!(
            resolve_auto(
                &[unknown_tools.clone(), no_tools.clone(), with_tools.clone()],
                &r
            )
            .as_deref(),
            Some("vendor/with-tools")
        );
        // Without tools in the request, tool support is not required.
        r.needs_tools = false;
        assert_eq!(
            resolve_auto(&[unknown_tools, no_tools], &r).as_deref(),
            Some("vendor/no-tools"),
            "tie on zero cost breaks by id"
        );
    }

    #[test]
    fn context_window_must_fit_estimate_plus_allowance() {
        let mut r = req();
        r.est_input_tokens = 10_000;
        r.output_allowance = 2_000;
        let mut small = model("vendor/small", "0", "0");
        small.context_length = Some(11_999);
        assert!(!is_eligible(&small, &r));
        let mut exact = model("vendor/exact", "0", "0");
        exact.context_length = Some(12_000);
        assert!(is_eligible(&exact, &r), "the boundary itself fits");
        assert_eq!(
            resolve_auto(&[small, exact], &r).as_deref(),
            Some("vendor/exact")
        );
    }

    #[test]
    fn known_image_and_request_fees_count() {
        let mut with_image_fee = model("vendor/a-flat", "0", "0");
        with_image_fee.pricing_image = Some("0.01".into());
        let flat = model("vendor/b-flat", "0", "0");

        // No images in the turn: the image fee is not applicable; id tie-break.
        let r = req();
        assert_eq!(
            resolve_auto(&[with_image_fee.clone(), flat.clone()], &r).as_deref(),
            Some("vendor/a-flat")
        );
        // Two images: the fee-free model wins.
        let mut images = req();
        images.needs_image = true;
        images.image_count = 2;
        assert_eq!(
            resolve_auto(&[with_image_fee.clone(), flat.clone()], &images).as_deref(),
            Some("vendor/b-flat")
        );
        // A known per-request fee participates too.
        let mut with_request_fee = model("vendor/c-request", "0", "0");
        with_request_fee.pricing_request = Some("0.04".into());
        assert_eq!(
            resolve_auto(&[with_request_fee, flat], &r).as_deref(),
            Some("vendor/b-flat")
        );
    }

    #[test]
    fn empty_catalog_resolves_to_none() {
        assert_eq!(resolve_auto(&[], &req()), None);
    }

    #[test]
    fn attachment_token_estimates() {
        assert_eq!(
            attachment_tokens(AttachmentShape::Image, Some(5_000_000)),
            IMAGE_INPUT_TOKENS
        );
        assert_eq!(
            attachment_tokens(AttachmentShape::Video, None),
            VIDEO_INPUT_TOKENS
        );
        assert_eq!(
            attachment_tokens(AttachmentShape::Pdf, Some(40_000)),
            10_000
        );
        assert_eq!(
            attachment_tokens(AttachmentShape::Pdf, None),
            PDF_FALLBACK_TOKENS
        );
        assert_eq!(attachment_tokens(AttachmentShape::Text, Some(400)), 100);
        assert_eq!(
            attachment_tokens(AttachmentShape::Text, None),
            TEXT_FALLBACK_TOKENS
        );
        assert_eq!(attachment_tokens(AttachmentShape::Skipped, Some(1_000)), 0);
        // The text cap matches the hydration limit.
        let over = crate::media_context::TEXT_LIMIT as u64 * 2;
        assert_eq!(
            attachment_tokens(AttachmentShape::Text, Some(over)),
            estimate_text_tokens(crate::media_context::TEXT_LIMIT as u64)
        );
    }
}
