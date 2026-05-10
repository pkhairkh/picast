//! boGDan Deobfuscation Pipeline
//!
//! A trait-based, pluggable deobfuscation system for video hosting sites.
//! Each deobfuscation step implements the [`DeobfuscationStep`] trait,
//! and a [`DeobfuscationPipeline`] chains multiple steps together.
//!
//! The pipeline is built from a [`ProviderConfig`](crate::provider::ProviderConfig)
//! at runtime, so adding new providers only requires a TOML config file
//! using existing step primitives.

use crate::provider::{ContentExtraction, DeobfuscationStep as StepDef, UrlExtractionRule};
use base64::Engine;
use regex_lite::Regex;
use scraper::{Html, Selector};
use serde_json::Value;

// ── Step Trait ───────────────────────────────────────────────────────

/// A single deobfuscation step that transforms an input string.
///
/// Each step takes a string and returns `Some(transformed_string)` on
/// success, or `None` if the step cannot process the input (e.g.,
/// invalid Base64). Steps are chained in order by the pipeline.
pub trait DeobfuscationStep: std::fmt::Debug + Send + Sync {
    /// Apply this step to the input string.
    fn apply(&self, input: &str) -> Option<String>;

    /// Human-readable name for logging.
    fn name(&self) -> &str;
}

// ── Concrete Step Implementations ────────────────────────────────────

/// ROT13 substitution cipher (letters only).
#[derive(Debug, Clone)]
pub struct Rot13Step;

impl DeobfuscationStep for Rot13Step {
    fn apply(&self, input: &str) -> Option<String> {
        Some(
            input
                .chars()
                .map(|ch| {
                    if ch.is_ascii_uppercase() {
                        let o = ch as u32;
                        char::from_u32(((o - 65 + 13) % 26) + 65).unwrap_or(ch)
                    } else if ch.is_ascii_lowercase() {
                        let o = ch as u32;
                        char::from_u32(((o - 97 + 13) % 26) + 97).unwrap_or(ch)
                    } else {
                        ch
                    }
                })
                .collect(),
        )
    }

    fn name(&self) -> &str {
        "rot13"
    }
}

/// Strip marker substrings used as obfuscation separators.
#[derive(Debug, Clone)]
pub struct StripMarkersStep {
    /// Patterns to remove from the input.
    pub patterns: Vec<String>,
}

impl DeobfuscationStep for StripMarkersStep {
    fn apply(&self, input: &str) -> Option<String> {
        let mut result = input.to_owned();
        for pat in &self.patterns {
            result = result.replace(pat.as_str(), "");
        }
        Some(result)
    }

    fn name(&self) -> &str {
        "strip_markers"
    }
}

/// Standard Base64 decode with safe padding.
#[derive(Debug, Clone)]
pub struct Base64DecodeStep;

impl DeobfuscationStep for Base64DecodeStep {
    fn apply(&self, input: &str) -> Option<String> {
        safe_b64_decode(input)
    }

    fn name(&self) -> &str {
        "base64_decode"
    }
}

/// Shift character code-points by a fixed amount (decode subtracts).
#[derive(Debug, Clone)]
pub struct CharShiftStep {
    /// Amount to shift. The decode operation subtracts this value
    /// from each character's code point.
    pub amount: u32,
}

impl DeobfuscationStep for CharShiftStep {
    fn apply(&self, input: &str) -> Option<String> {
        Some(
            input
                .chars()
                .map(|c| {
                    let code = c as u32;
                    if code >= self.amount {
                        char::from_u32(code - self.amount).unwrap_or(c)
                    } else {
                        c
                    }
                })
                .collect(),
        )
    }

    fn name(&self) -> &str {
        "char_shift"
    }
}

/// Reverse the string.
#[derive(Debug, Clone)]
pub struct ReverseStep;

impl DeobfuscationStep for ReverseStep {
    fn apply(&self, input: &str) -> Option<String> {
        Some(input.chars().rev().collect())
    }

    fn name(&self) -> &str {
        "reverse"
    }
}

/// Parse the string as JSON and extract a value at a key path.
#[derive(Debug, Clone)]
pub struct JsonParseStep {
    /// Key path with dot notation (e.g., "data.url").
    pub key: String,
}

impl DeobfuscationStep for JsonParseStep {
    fn apply(&self, input: &str) -> Option<String> {
        let parsed: Value = serde_json::from_str(input).ok()?;
        let value = get_json_by_path(&parsed, &self.key)?;
        if let Some(s) = value.as_str() {
            Some(s.to_owned())
        } else {
            // Return the JSON string of the value (for objects/arrays)
            serde_json::to_string(value).ok()
        }
    }

    fn name(&self) -> &str {
        "json_parse"
    }
}

/// Extract a substring using a regex. Returns the first capture group.
#[derive(Debug, Clone)]
pub struct RegexExtractStep {
    /// Compiled regex pattern.
    pub pattern: Regex,
}

impl DeobfuscationStep for RegexExtractStep {
    fn apply(&self, input: &str) -> Option<String> {
        let caps = self.pattern.captures(input)?;
        caps.get(1).map(|m| m.as_str().to_owned())
    }

    fn name(&self) -> &str {
        "regex_extract"
    }
}

/// Clean and pad a Base64 string (strip backslashes, add padding).
#[derive(Debug, Clone)]
pub struct CleanBase64Step;

impl DeobfuscationStep for CleanBase64Step {
    fn apply(&self, input: &str) -> Option<String> {
        let cleaned = input.replace('\\', "");
        let pad = cleaned.len() % 4;
        let padded = if pad > 0 {
            let mut s = cleaned;
            for _ in 0..(4 - pad) {
                s.push('=');
            }
            s
        } else {
            cleaned
        };
        // Validate it decodes
        base64::engine::general_purpose::STANDARD
            .decode(&padded)
            .ok()?;
        Some(padded)
    }

    fn name(&self) -> &str {
        "clean_base64"
    }
}

/// Strip underscores from the string.
#[derive(Debug, Clone)]
pub struct StripUnderscoresStep;

impl DeobfuscationStep for StripUnderscoresStep {
    fn apply(&self, input: &str) -> Option<String> {
        Some(input.replace('_', ""))
    }

    fn name(&self) -> &str {
        "strip_underscores"
    }
}

// ── Pipeline ─────────────────────────────────────────────────────────

/// An ordered chain of deobfuscation steps.
///
/// The pipeline takes raw obfuscated input and applies each step
/// sequentially. If any step returns `None`, the pipeline fails.
#[derive(Debug, Default)]
pub struct DeobfuscationPipeline {
    steps: Vec<Box<dyn DeobfuscationStep>>,
}

impl DeobfuscationPipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a step to the end of the pipeline.
    pub fn add_step(&mut self, step: Box<dyn DeobfuscationStep>) {
        self.steps.push(step);
    }

    /// Run the pipeline on the given input.
    ///
    /// Returns `Some(deobfuscated_string)` if all steps succeed,
    /// or `None` if any step fails.
    pub fn run(&self, input: &str) -> Option<String> {
        let mut current = input.to_owned();
        for step in &self.steps {
            match step.apply(&current) {
                Some(output) => {
                    tracing::trace!(step = step.name(), "deobfuscation step succeeded");
                    current = output;
                }
                None => {
                    tracing::debug!(
                        step = step.name(),
                        "deobfuscation step failed — pipeline aborted"
                    );
                    return None;
                }
            }
        }
        Some(current)
    }

    /// Number of steps in the pipeline.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ── Build Pipeline from Config ───────────────────────────────────────

/// Build a [`DeobfuscationPipeline`] from a list of step definitions.
pub fn build_pipeline(step_defs: &[StepDef]) -> DeobfuscationPipeline {
    let mut pipeline = DeobfuscationPipeline::new();
    for def in step_defs {
        if let Some(step) = build_step(def) {
            pipeline.add_step(step);
        } else {
            tracing::warn!(step = ?def, "skipping unrecognized deobfuscation step");
        }
    }
    pipeline
}

/// Build a single step from a step definition.
pub fn build_step(def: &StepDef) -> Option<Box<dyn DeobfuscationStep>> {
    match def {
        StepDef::Rot13 => Some(Box::new(Rot13Step)),
        StepDef::StripMarkers { patterns } => Some(Box::new(StripMarkersStep {
            patterns: patterns.clone(),
        })),
        StepDef::Base64Decode => Some(Box::new(Base64DecodeStep)),
        StepDef::CharShift { amount } => Some(Box::new(CharShiftStep { amount: *amount })),
        StepDef::Reverse => Some(Box::new(ReverseStep)),
        StepDef::JsonParse { key } => Some(Box::new(JsonParseStep { key: key.clone() })),
        StepDef::RegexExtract { pattern } => {
            let re = Regex::new(pattern).ok()?;
            Some(Box::new(RegexExtractStep { pattern: re }))
        }
        StepDef::CleanBase64 => Some(Box::new(CleanBase64Step)),
        StepDef::StripUnderscores => Some(Box::new(StripUnderscoresStep)),
    }
}

// ── Content Extraction ───────────────────────────────────────────────

/// Extract obfuscated data from HTML using a content extraction method.
pub fn extract_content(html: &str, extraction: &ContentExtraction) -> Option<String> {
    match extraction {
        ContentExtraction::ScriptJsonTag => {
            let document = Html::parse_document(html);
            let selector = Selector::parse("script[type='application/json']").ok()?;
            for element in document.select(&selector) {
                let raw: String = element.text().collect();
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
            None
        }
        ContentExtraction::JsVariable { pattern } => {
            let re = Regex::new(pattern).ok()?;
            let cap = re.captures(html)?;
            cap.get(1).map(|m| m.as_str().to_owned())
        }
        ContentExtraction::CssSelector {
            selector,
            attribute,
        } => {
            let document = Html::parse_document(html);
            let sel = Selector::parse(selector).ok()?;
            for element in document.select(&sel) {
                if let Some(attr) = attribute {
                    if let Some(value) = element.value().attr(attr) {
                        if !value.is_empty() {
                            return Some(value.to_owned());
                        }
                    }
                } else {
                    let text: String = element.text().collect();
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_owned());
                    }
                }
            }
            None
        }
        ContentExtraction::RegexMatch { pattern } => {
            let re = Regex::new(pattern).ok()?;
            let cap = re.captures(html)?;
            cap.get(1).map(|m| m.as_str().to_owned())
        }
        ContentExtraction::JsRedirectThen { inner } => {
            // JS redirect is handled at the resolver level, not here.
            // This extraction method is a marker for the resolver to
            // follow the redirect first, then apply the inner extraction.
            extract_content(html, inner)
        }
    }
}

// ── URL Extraction from Deobfuscated JSON ────────────────────────────

/// Result of extracting a URL from deobfuscated data.
#[derive(Debug, Clone)]
pub struct ExtractedUrl {
    /// The media URL.
    pub url: String,
    /// The request token (if extracted from the JSON's "request" field).
    pub request_token: Option<String>,
    /// The file code (if extracted from the JSON's "file_code" field).
    pub file_code: Option<String>,
}

/// Extract a media URL from deobfuscated JSON using extraction rules.
pub fn extract_url_from_deobfuscated(
    deobfuscated: &str,
    rules: &[UrlExtractionRule],
    bait_domains: &[String],
    bait_filenames: &[String],
) -> Option<ExtractedUrl> {
    let parsed: Value = serde_json::from_str(deobfuscated).ok()?;
    let obj = parsed.as_object()?;

    // Extract common fields
    let request_token = obj
        .get("request")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());

    let file_code = obj
        .get("file_code")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());

    // Sort rules by priority
    let mut sorted_rules: Vec<_> = rules.iter().enumerate().collect();
    sorted_rules.sort_by_key(|(_, rule)| match rule {
        UrlExtractionRule::JsonKey { priority, .. } => *priority,
        UrlExtractionRule::RegexUrl { priority, .. } => *priority,
    });

    for (_, rule) in &sorted_rules {
        match rule {
            UrlExtractionRule::JsonKey {
                key,
                append_rq_token,
                prefer_mp4,
                ..
            } => {
                if let Some(value) = obj.get(key) {
                    if let Some(url) = extract_url_from_json_value(
                        value,
                        *prefer_mp4,
                        bait_domains,
                        bait_filenames,
                    ) {
                        let url = if *append_rq_token {
                            if let Some(ref token) = request_token {
                                append_rq(&url, token)
                            } else {
                                url
                            }
                        } else {
                            url
                        };
                        return Some(ExtractedUrl {
                            url,
                            request_token,
                            file_code,
                        });
                    }
                }
            }
            UrlExtractionRule::RegexUrl { pattern, .. } => {
                let text = deobfuscated;
                if let Ok(re) = Regex::new(pattern) {
                    if let Some(cap) = re.captures(text) {
                        if let Some(m) = cap.get(1) {
                            let url = m.as_str().to_owned();
                            if !is_bait_source(&url, bait_domains, bait_filenames) {
                                return Some(ExtractedUrl {
                                    url,
                                    request_token,
                                    file_code,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Extract a URL from a JSON value, handling strings and quality-level objects.
fn extract_url_from_json_value(
    value: &Value,
    prefer_mp4: bool,
    bait_domains: &[String],
    bait_filenames: &[String],
) -> Option<String> {
    // Case 1: Simple string URL
    if let Some(url) = value.as_str() {
        if !url.is_empty() && !is_bait_source(url, bait_domains, bait_filenames) {
            return Some(url.to_owned());
        }
        return None;
    }

    // Case 2: Object with quality levels
    if let Some(obj) = value.as_object() {
        let mut mp4_candidates: Vec<(&str, &str)> = Vec::new();
        let mut hls_candidates: Vec<(&str, &str)> = Vec::new();

        for (key, url_val) in obj.iter() {
            if let Some(url) = url_val.as_str() {
                if !url.is_empty() && !is_bait_source(url, bait_domains, bait_filenames) {
                    if url.to_lowercase().contains(".m3u8") {
                        hls_candidates.push((key, url));
                    } else {
                        mp4_candidates.push((key, url));
                    }
                }
            }
        }

        if prefer_mp4 && !mp4_candidates.is_empty() {
            let quality_rank = |q: &str| -> i32 {
                match q {
                    "720" => 0,
                    "480" => 1,
                    "360" => 2,
                    "240" => 3,
                    "1080" => 4,
                    _ => 5,
                }
            };
            mp4_candidates.sort_by_key(|(q, _)| quality_rank(q));
            return Some(mp4_candidates[0].1.to_owned());
        }

        if !hls_candidates.is_empty() {
            let quality_rank = |q: &str| -> i32 {
                match q {
                    "720" => 0,
                    "480" => 1,
                    "360" => 2,
                    "240" => 3,
                    "1080" => 4,
                    _ => 5,
                }
            };
            hls_candidates.sort_by_key(|(q, _)| quality_rank(q));
            return Some(hls_candidates[0].1.to_owned());
        }

        if !mp4_candidates.is_empty() {
            return Some(mp4_candidates[0].1.to_owned());
        }
    }

    None
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Base64 decode with safe padding.
fn safe_b64_decode(s: &str) -> Option<String> {
    let padded = {
        let pad = s.len() % 4;
        let mut s = s.to_owned();
        if pad > 0 {
            for _ in 0..(4 - pad) {
                s.push('=');
            }
        }
        s
    };
    base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Get a JSON value by dot-separated key path.
fn get_json_by_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    Some(current)
}

/// Append `&rq=<token>` to a URL if not already present.
fn append_rq(url: &str, token: &str) -> String {
    if url.contains("&rq=") || url.contains("?rq=") || token.is_empty() {
        return url.to_owned();
    }
    if url.contains('?') {
        format!("{}&rq={}", url, token)
    } else {
        format!("{}?rq={}", url, token)
    }
}

/// Check if a URL is a bait/test video source.
fn is_bait_source(source: &str, bait_domains: &[String], bait_filenames: &[String]) -> bool {
    let lower = source.to_lowercase();
    if bait_filenames
        .iter()
        .any(|fn_| lower.contains(&fn_.to_lowercase()))
    {
        return true;
    }
    if let Ok(parsed) = url::Url::parse(source) {
        if let Some(host) = parsed.host_str() {
            if bait_domains.iter().any(|d| host.contains(d.as_str())) {
                return true;
            }
        }
    }
    false
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rot13_step() {
        let step = Rot13Step;
        assert_eq!(step.apply("Hello"), Some("Uryyb".into()));
        assert_eq!(step.apply("Uryyb"), Some("Hello".into()));
        assert_eq!(step.apply("abc123"), Some("nop123".into()));
        assert_eq!(step.name(), "rot13");
    }

    #[test]
    fn strip_markers_step() {
        let step = StripMarkersStep {
            patterns: vec![
                "@$".into(),
                "^^".into(),
                "~@".into(),
                "%?".into(),
                "*~".into(),
                "!!".into(),
                "#&".into(),
            ],
        };
        let result = step.apply("DROH#&nJjm^^AJIg%?BSMq~@AGkj").unwrap();
        assert!(!result.contains("#&"));
        assert!(!result.contains("^^"));
        assert!(!result.contains("%?"));
        assert!(!result.contains("~@"));
    }

    #[test]
    fn base64_decode_step() {
        let step = Base64DecodeStep;
        assert_eq!(step.apply("SGVsbG8="), Some("Hello".into()));
        assert_eq!(step.apply("SGVsbG8"), Some("Hello".into()));
        assert!(step.apply("!!!invalid!!!").is_none());
    }

    #[test]
    fn char_shift_step() {
        let step = CharShiftStep { amount: 3 };
        assert_eq!(step.apply("def"), Some("abc".into()));
        // 'A'(65) - 3 = 62 = '>'
        assert_eq!(step.apply("ABC"), Some(">?@".into()));
    }

    #[test]
    fn reverse_step() {
        let step = ReverseStep;
        assert_eq!(step.apply("abc"), Some("cba".into()));
        assert_eq!(step.apply(""), Some("".into()));
    }

    #[test]
    fn json_parse_step() {
        let step = JsonParseStep { key: "url".into() };
        let input = r#"{"url": "https://cdn.example.com/video.mp4"}"#;
        assert_eq!(
            step.apply(input),
            Some("https://cdn.example.com/video.mp4".into())
        );
    }

    #[test]
    fn json_parse_step_nested() {
        let step = JsonParseStep {
            key: "data.url".into(),
        };
        let input = r#"{"data": {"url": "https://cdn.example.com/video.mp4"}}"#;
        assert_eq!(
            step.apply(input),
            Some("https://cdn.example.com/video.mp4".into())
        );
    }

    #[test]
    fn json_parse_step_missing_key() {
        let step = JsonParseStep {
            key: "missing".into(),
        };
        let input = r#"{"url": "https://cdn.example.com/video.mp4"}"#;
        assert!(step.apply(input).is_none());
    }

    #[test]
    fn regex_extract_step() {
        let step = RegexExtractStep {
            pattern: Regex::new(r#"MKGMa="(.*?)""#).unwrap(),
        };
        let input = r#"MKGMa="abc123""#;
        assert_eq!(step.apply(input), Some("abc123".into()));
    }

    #[test]
    fn clean_base64_step() {
        let step = CleanBase64Step;
        let input = r#"SGVsbG8"#;
        let result = step.apply(input);
        assert!(result.is_some());
    }

    #[test]
    fn strip_underscores_step() {
        let step = StripUnderscoresStep;
        assert_eq!(step.apply("a_b_c"), Some("abc".into()));
        assert_eq!(
            step.apply("no_underscores_here"),
            Some("nounderscoreshere".into())
        );
    }

    #[test]
    fn pipeline_chains_steps() {
        let mut pipeline = DeobfuscationPipeline::new();
        pipeline.add_step(Box::new(ReverseStep));
        pipeline.add_step(Box::new(CharShiftStep { amount: 1 }));

        // "bcd" → reverse → "dcb" → shift(1) → "cba"
        assert_eq!(pipeline.run("bcd"), Some("cba".into()));
    }

    #[test]
    fn pipeline_fails_on_step_failure() {
        let mut pipeline = DeobfuscationPipeline::new();
        pipeline.add_step(Box::new(ReverseStep));
        pipeline.add_step(Box::new(Base64DecodeStep));

        let result = pipeline.run("not base64 at all!!!");
        assert!(result.is_none());
    }

    #[test]
    fn build_pipeline_from_config() {
        let steps = vec![
            StepDef::Rot13,
            StepDef::StripMarkers {
                patterns: vec!["@$".into(), "^^".into()],
            },
            StepDef::Base64Decode,
            StepDef::CharShift { amount: 3 },
            StepDef::Reverse,
            StepDef::Base64Decode,
        ];

        let pipeline = build_pipeline(&steps);
        assert_eq!(pipeline.len(), 6);
    }

    #[test]
    fn extract_content_script_json_tag() {
        let html = r#"<html><script type="application/json">["obfuscated_data"]</script></html>"#;
        let result = extract_content(html, &ContentExtraction::ScriptJsonTag);
        assert_eq!(result, Some(r#"["obfuscated_data"]"#.to_string()));
    }

    #[test]
    fn extract_content_js_variable() {
        let html = r#"<script>MKGMa="abc123"</script>"#;
        let result = extract_content(
            html,
            &ContentExtraction::JsVariable {
                pattern: r#"MKGMa="(.*?)""#.into(),
            },
        );
        assert_eq!(result, Some("abc123".into()));
    }

    #[test]
    fn extract_content_regex_match() {
        let html = r#"a168c = 'base64data'"#;
        let result = extract_content(
            html,
            &ContentExtraction::RegexMatch {
                pattern: r#"a168c\s*=\s*'([^']+)'"#.into(),
            },
        );
        assert_eq!(result, Some("base64data".into()));
    }

    #[test]
    fn extract_url_from_deobfuscated_with_json_key() {
        let json = r#"{"mp4": "https://cdn.example.com/video.mp4", "source": "https://cdn.example.com/stream.m3u8"}"#;
        let rules = vec![
            UrlExtractionRule::JsonKey {
                key: "mp4".into(),
                priority: 1,
                append_rq_token: false,
                prefer_mp4: true,
            },
            UrlExtractionRule::JsonKey {
                key: "source".into(),
                priority: 2,
                append_rq_token: false,
                prefer_mp4: true,
            },
        ];
        let result = extract_url_from_deobfuscated(json, &rules, &[], &[]);
        assert!(result.is_some());
        assert_eq!(result.unwrap().url, "https://cdn.example.com/video.mp4");
    }

    #[test]
    fn extract_url_with_rq_token() {
        let json = r#"{"mp4": "https://cdn.example.com/video.mp4", "request": "TOKEN123"}"#;
        let rules = vec![UrlExtractionRule::JsonKey {
            key: "mp4".into(),
            priority: 1,
            append_rq_token: true,
            prefer_mp4: true,
        }];
        let result = extract_url_from_deobfuscated(json, &rules, &[], &[]);
        assert!(result.is_some());
        let extracted = result.unwrap();
        assert_eq!(
            extracted.url,
            "https://cdn.example.com/video.mp4?rq=TOKEN123"
        );
        assert_eq!(extracted.request_token, Some("TOKEN123".into()));
    }

    #[test]
    fn extract_url_skips_bait() {
        let json = r#"{"mp4": "https://test-videos.co.uk/Big_Buck_Bunny_1080_10s_5MB.mp4"}"#;
        let rules = vec![UrlExtractionRule::JsonKey {
            key: "mp4".into(),
            priority: 1,
            append_rq_token: false,
            prefer_mp4: true,
        }];
        let bait_domains = vec!["test-videos.co.uk".into()];
        let bait_filenames = vec!["BigBuckBunny".into()];
        let result = extract_url_from_deobfuscated(json, &rules, &bait_domains, &bait_filenames);
        assert!(result.is_none());
    }

    #[test]
    fn append_rq_helper() {
        assert_eq!(
            append_rq("https://cdn.example.com/video.mp4?t=abc", "TOKEN123"),
            "https://cdn.example.com/video.mp4?t=abc&rq=TOKEN123"
        );
        assert_eq!(
            append_rq(
                "https://cdn.example.com/video.mp4?t=abc&rq=EXISTING",
                "TOKEN123"
            ),
            "https://cdn.example.com/video.mp4?t=abc&rq=EXISTING"
        );
        assert_eq!(
            append_rq("https://cdn.example.com/video.mp4", "TOKEN123"),
            "https://cdn.example.com/video.mp4?rq=TOKEN123"
        );
        assert_eq!(
            append_rq("https://cdn.example.com/video.mp4", ""),
            "https://cdn.example.com/video.mp4"
        );
    }
}
