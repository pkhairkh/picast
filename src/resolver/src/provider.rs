//! boGDan Provider Configuration
//!
//! Config-driven video hosting provider definitions. Each provider is
//! described by a TOML file under `providers.d/` that specifies:
//!
//! - Domain patterns for URL matching
//! - Deobfuscation pipeline steps
//! - URL extraction rules
//! - CDN handling and request headers
//!
//! Adding a new provider that uses existing deobfuscation primitives
//! requires ONLY a new `.toml` file — no Rust code changes.

use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

// ── Errors ───────────────────────────────────────────────────────────

/// Errors during provider configuration loading or validation.
#[derive(Error, Debug)]
pub enum ProviderConfigError {
    /// The TOML file could not be read.
    #[error("failed to read provider config {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The TOML content could not be parsed.
    #[error("failed to parse provider config {path}: {source}")]
    TomlParse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    /// A required field is missing.
    #[error("provider config {path}: missing required field '{field}'")]
    MissingField { path: String, field: String },

    /// A regex pattern is invalid.
    #[error("provider config {path}: invalid regex in {context}: {pattern}")]
    InvalidRegex { path: String, context: String, pattern: String },

    /// A deobfuscation step has invalid parameters.
    #[error("provider config {path}: invalid deobfuscation step '{step}': {reason}")]
    InvalidStep { path: String, step: String, reason: String },

    /// Duplicate provider name found.
    #[error("duplicate provider name '{name}' in {path1} and {path2}")]
    DuplicateName { name: String, path1: String, path2: String },

    /// The deobfuscation pipeline is empty.
    #[error("provider config {path}: deobfuscation pipeline is empty")]
    EmptyPipeline { path: String },
}

// ── Domain Pattern ───────────────────────────────────────────────────

/// How to match a domain against a pattern.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DomainMatchKind {
    /// Exact string match (e.g., "voe.sx").
    Exact,
    /// Suffix match — matches the domain or any subdomain (e.g., "voe.sx"
    /// matches "sub.voe.sx").
    #[default]
    Suffix,
    /// Regex match — the pattern is a full regex applied to the hostname.
    Regex,
}

/// A single domain pattern for URL matching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomainPattern {
    /// The pattern string (exact domain, suffix, or regex).
    pub pattern: String,
    /// How to interpret the pattern.
    #[serde(default)]
    pub kind: DomainMatchKind,
}

impl DomainPattern {
    /// Check if a hostname matches this pattern.
    pub fn matches(&self, host: &str) -> bool {
        let host_lower = host.to_lowercase();
        let pattern_lower = self.pattern.to_lowercase();
        match self.kind {
            DomainMatchKind::Exact => host_lower == pattern_lower,
            DomainMatchKind::Suffix => {
                host_lower == pattern_lower || host_lower.ends_with(&format!(".{}", pattern_lower))
            },
            DomainMatchKind::Regex => {
                if let Ok(re) = Regex::new(&self.pattern) {
                    re.is_match(host)
                } else {
                    false
                }
            },
        }
    }

    /// Validate that a regex pattern compiles.
    pub fn validate_regex(&self) -> Result<(), ProviderConfigError> {
        if self.kind == DomainMatchKind::Regex && Regex::new(&self.pattern).is_err() {
            return Err(ProviderConfigError::InvalidRegex {
                path: String::new(),
                context: "domain pattern".into(),
                pattern: self.pattern.clone(),
            });
        }
        Ok(())
    }
}

// ── Deobfuscation Steps ──────────────────────────────────────────────

/// A single step in a deobfuscation pipeline.
///
/// Each variant corresponds to a primitive operation that video hosting
/// sites use to obfuscate their media URLs. Steps are chained together
/// in the order they appear in the provider config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeobfuscationStep {
    /// ROT13 substitution cipher (letters only).
    Rot13,

    /// Strip marker patterns used as obfuscation separators.
    /// The `patterns` field lists the substrings to remove.
    StripMarkers { patterns: Vec<String> },

    /// Standard Base64 decode with safe padding.
    Base64Decode,

    /// Shift character code-points by a fixed amount.
    /// `amount` is the positive shift value; the decode subtracts it.
    CharShift { amount: u32 },

    /// Reverse the string.
    Reverse,

    /// Parse the string as JSON and extract a value at the given key path.
    /// Key path supports dot notation (e.g., "data.url").
    JsonParse { key: String },

    /// Extract a substring using a regex. Returns the first capture group.
    RegexExtract { pattern: String },

    /// Clean and pad a Base64 string (strip backslashes, add padding).
    CleanBase64,

    /// Strip underscores from the string (used by Voe Method 7).
    StripUnderscores,
}

// ── URL Extraction Rules ─────────────────────────────────────────────

/// How to extract the media URL from deobfuscated data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UrlExtractionRule {
    /// Extract a value from JSON at the given key. If the value is a string,
    /// return it directly. If it's an object with quality levels, select
    /// based on quality preference and CDN rate limits.
    JsonKey {
        /// JSON key to look up (e.g., "mp4", "source", "direct_access_url").
        key: String,
        /// Priority: lower numbers are tried first.
        priority: u32,
        /// Whether to append the `request` token as `&rq=` to extracted URLs.
        #[serde(default)]
        append_rq_token: bool,
        /// Whether to skip HLS URLs (.m3u8) from this key.
        #[serde(default = "default_true")]
        prefer_mp4: bool,
    },

    /// Extract a URL using a regex. Returns the first capture group.
    RegexUrl { pattern: String, priority: u32 },
}

fn default_true() -> bool {
    true
}

// ── CDN / Quality Settings ───────────────────────────────────────────

/// Quality preference order for multi-quality media objects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QualityPreference {
    /// Quality label (e.g., "720", "1080", "480").
    pub label: String,
    /// Rank: 0 = most preferred. Lower is better.
    pub rank: u32,
    /// Typical bitrate in kbps for this quality level.
    pub typical_bitrate_kbps: Option<u64>,
}

/// CDN-specific settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CdnConfig {
    /// Query parameter name for CDN speed limit (e.g., "sp").
    #[serde(default = "default_sp_param")]
    pub speed_limit_param: String,

    /// Whether to send an activation POST before downloading.
    #[serde(default)]
    pub requires_session_activation: bool,

    /// Path for the session activation POST (relative to domain root).
    #[serde(default = "default_engine_update")]
    pub activation_path: String,

    /// Obfuscation method for the activation payload.
    #[serde(default)]
    pub activation_obfuscation: ActivationObfuscation,
}

fn default_sp_param() -> String {
    "sp".into()
}

fn default_engine_update() -> String {
    "/engine/update".into()
}

impl Default for CdnConfig {
    fn default() -> Self {
        Self {
            speed_limit_param: default_sp_param(),
            requires_session_activation: false,
            activation_path: default_engine_update(),
            activation_obfuscation: ActivationObfuscation::default(),
        }
    }
}

/// How to obfuscate the session activation payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivationObfuscation {
    /// No obfuscation — send payload as plain JSON.
    #[default]
    None,
    /// Voe-style: JSON → Base64 → Reverse → CharShift(+3).
    VoeEngineUpdate,
}

// ── Provider Config ──────────────────────────────────────────────────

/// Top-level configuration for a video hosting provider.
///
/// Each provider is defined in a TOML file under `providers.d/`.
/// The filename (without `.toml`) becomes the provider's unique ID.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    /// Human-readable provider name (e.g., "Voe", "DoodStream").
    pub name: String,

    /// Whether this provider is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Domain patterns for matching URLs to this provider.
    pub domain_patterns: Vec<DomainPattern>,

    /// The resolver type to use.
    #[serde(default = "default_resolver_type")]
    pub resolver_type: ResolverType,

    /// Deobfuscation pipeline: ordered list of steps to apply to the
    /// obfuscated data extracted from the page.
    #[serde(default)]
    pub deobfuscation_pipeline: Vec<DeobfuscationPipelineEntry>,

    /// Rules for extracting the media URL from the deobfuscated data.
    #[serde(default)]
    pub url_extraction: Vec<UrlExtractionRule>,

    /// Quality preference order for multi-quality objects.
    #[serde(default)]
    pub quality_preferences: Vec<QualityPreference>,

    /// CDN-specific settings.
    #[serde(default)]
    pub cdn: CdnConfig,

    /// Whether to forward cookies from page fetch to download.
    #[serde(default)]
    pub forward_cookies: bool,

    /// Custom headers to send with page fetches.
    #[serde(default)]
    pub request_headers: HashMap<String, String>,

    /// Request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Known bait/test video domains and filenames.
    #[serde(default)]
    pub bait_domains: Vec<String>,

    /// Bait filenames to detect decoy sources.
    #[serde(default)]
    pub bait_filenames: Vec<String>,

    /// Content extraction methods — how to find the obfuscated data
    /// in the page HTML.
    #[serde(default)]
    pub content_extraction: Vec<ContentExtraction>,

    /// Post-extraction hooks — actions to take after successful resolution.
    #[serde(default)]
    pub post_resolve: Vec<PostResolveAction>,

    /// URL transformation rules (e.g., /d/ → /e/ for DoodStream).
    #[serde(default)]
    pub url_transforms: Vec<UrlTransform>,
}

/// Type of resolver to use for this provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResolverType {
    /// Custom deobfuscation pipeline defined in this config.
    #[default]
    Custom,
    /// Delegate to yt-dlp subprocess.
    YtDlp,
    /// Return the URL as-is (no resolution needed).
    Passthrough,
}

/// An entry in the deobfuscation pipeline, associating a name with a step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeobfuscationPipelineEntry {
    /// Human-readable name for logging (e.g., "method8_json").
    #[serde(default)]
    pub name: String,

    /// How to extract the obfuscated data from the page.
    pub extraction: ContentExtraction,

    /// Ordered list of deobfuscation steps.
    pub steps: Vec<DeobfuscationStep>,

    /// Rules for extracting the media URL from the deobfuscated data.
    pub url_extraction: Vec<UrlExtractionRule>,

    /// Whether to check for bait sources after extraction.
    #[serde(default = "default_true")]
    pub check_bait: bool,
}

/// How to extract obfuscated data from a page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ContentExtraction {
    /// Extract text content from `<script type="application/json">` tags.
    ScriptJsonTag,

    /// Extract the value of a JavaScript variable using regex.
    JsVariable {
        /// Regex with one capture group for the value.
        pattern: String,
    },

    /// Extract from an HTML element using a CSS selector.
    CssSelector {
        /// CSS selector string.
        selector: String,
        /// Attribute to read (None = text content).
        attribute: Option<String>,
    },

    /// Extract from a regex match in the raw HTML.
    RegexMatch {
        /// Regex with one capture group.
        pattern: String,
    },

    /// Follow a JS redirect first, then apply the inner extraction.
    JsRedirectThen {
        /// The extraction method to apply after following the redirect.
        inner: Box<ContentExtraction>,
    },
}

/// An action to take after successful resolution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PostResolveAction {
    /// Send an activation POST to the CDN (e.g., Voe's /engine/update).
    CdnSessionActivation,

    /// Append a query parameter from a JSON key in the deobfuscated data.
    AppendQueryFromJson {
        /// JSON key to read the value from (e.g., "request" for Voe's &rq= token).
        json_key: String,
        /// Query parameter name to append (e.g., "rq").
        param_name: String,
    },
}

/// A URL transformation rule (e.g., DoodStream's /d/ → /e/).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UrlTransform {
    /// Regex to match the URL path.
    pub pattern: String,
    /// Replacement string (supports $1, $2, etc. for capture groups).
    pub replacement: String,
    /// Description for logging.
    #[serde(default)]
    pub description: String,
}

fn default_resolver_type() -> ResolverType {
    ResolverType::Custom
}

fn default_timeout_secs() -> u64 {
    15
}

impl ProviderConfig {
    /// Load a provider config from a TOML file.
    pub fn load_from_file(path: &Path) -> Result<Self, ProviderConfigError> {
        let path_str = path.to_string_lossy().to_string();
        let content = std::fs::read_to_string(path)
            .map_err(|e| ProviderConfigError::Io { path: path_str.clone(), source: e })?;
        let config: ProviderConfig = toml::from_str(&content)
            .map_err(|e| ProviderConfigError::TomlParse { path: path_str, source: e })?;
        Ok(config)
    }

    /// Parse a provider config from a TOML string.
    pub fn from_toml_str(
        content: &str,
        path_for_errors: &str,
    ) -> Result<Self, ProviderConfigError> {
        toml::from_str(content)
            .map_err(|e| ProviderConfigError::TomlParse { path: path_for_errors.into(), source: e })
    }

    /// Validate the provider config, returning errors for any issues.
    pub fn validate(&self, path: &str) -> Result<(), ProviderConfigError> {
        // Check required fields
        if self.name.is_empty() {
            return Err(ProviderConfigError::MissingField {
                path: path.into(),
                field: "name".into(),
            });
        }
        if self.domain_patterns.is_empty() {
            return Err(ProviderConfigError::MissingField {
                path: path.into(),
                field: "domain_patterns".into(),
            });
        }

        // Validate domain pattern regexes
        for dp in &self.domain_patterns {
            if let Err(_e) = dp.validate_regex() {
                return Err(ProviderConfigError::InvalidRegex {
                    path: path.into(),
                    context: "domain pattern".into(),
                    pattern: dp.pattern.clone(),
                });
            }
        }

        // Validate deobfuscation pipeline entries
        for (i, entry) in self.deobfuscation_pipeline.iter().enumerate() {
            if entry.steps.is_empty() {
                return Err(ProviderConfigError::EmptyPipeline { path: path.into() });
            }
            // Validate regex patterns in steps
            for (j, step) in entry.steps.iter().enumerate() {
                if let DeobfuscationStep::RegexExtract { pattern } = step {
                    if Regex::new(pattern).is_err() {
                        return Err(ProviderConfigError::InvalidRegex {
                            path: path.into(),
                            context: format!("pipeline[{}].steps[{}]", i, j),
                            pattern: pattern.clone(),
                        });
                    }
                }
            }
            // Validate extraction regexes
            match &entry.extraction {
                ContentExtraction::JsVariable { pattern }
                | ContentExtraction::RegexMatch { pattern }
                    if Regex::new(pattern).is_err() =>
                {
                    return Err(ProviderConfigError::InvalidRegex {
                        path: path.into(),
                        context: format!("pipeline[{}].extraction", i),
                        pattern: pattern.clone(),
                    });
                },
                ContentExtraction::JsRedirectThen { inner } => match inner.as_ref() {
                    ContentExtraction::JsVariable { pattern }
                    | ContentExtraction::RegexMatch { pattern }
                        if Regex::new(pattern).is_err() =>
                    {
                        return Err(ProviderConfigError::InvalidRegex {
                            path: path.into(),
                            context: format!("pipeline[{}].extraction.redirect", i),
                            pattern: pattern.clone(),
                        });
                    },
                    _ => {},
                },
                _ => {},
            }
            // Validate url_extraction regexes
            for (j, rule) in entry.url_extraction.iter().enumerate() {
                if let UrlExtractionRule::RegexUrl { pattern, .. } = rule {
                    if Regex::new(pattern).is_err() {
                        return Err(ProviderConfigError::InvalidRegex {
                            path: path.into(),
                            context: format!("pipeline[{}].url_extraction[{}]", i, j),
                            pattern: pattern.clone(),
                        });
                    }
                }
            }
        }

        // Validate URL transforms
        for (i, transform) in self.url_transforms.iter().enumerate() {
            if Regex::new(&transform.pattern).is_err() {
                return Err(ProviderConfigError::InvalidRegex {
                    path: path.into(),
                    context: format!("url_transforms[{}]", i),
                    pattern: transform.pattern.clone(),
                });
            }
        }

        Ok(())
    }

    /// Check if a hostname matches any of this provider's domain patterns.
    pub fn matches_domain(&self, host: &str) -> bool {
        self.domain_patterns.iter().any(|dp| dp.matches(host))
    }
}

// ── Provider Registry ────────────────────────────────────────────────

/// Registry of all loaded provider configurations.
///
/// Loads all `.toml` files from a `providers.d/` directory at startup,
/// validates them, and provides lookup by domain.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    /// Providers indexed by their config file stem (e.g., "voe").
    providers: Vec<(String, ProviderConfig)>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load all `.toml` files from the given directory.
    ///
    /// Each file defines one provider. The filename (without `.toml`)
    /// becomes the provider's unique ID.
    pub fn load_from_dir(dir: &Path) -> Result<Self, ProviderConfigError> {
        let mut registry = Self::new();
        let dir_str = dir.to_string_lossy().to_string();

        let entries = std::fs::read_dir(dir)
            .map_err(|e| ProviderConfigError::Io { path: dir_str, source: e })?;

        for entry in entries {
            let entry = entry
                .map_err(|e| ProviderConfigError::Io { path: "<read_dir>".into(), source: e })?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();

                let config = ProviderConfig::load_from_file(&path)?;
                let path_str = path.to_string_lossy().to_string();

                // Validate
                config.validate(&path_str)?;

                if !config.enabled {
                    tracing::info!(
                        provider = %id,
                        name = %config.name,
                        "skipping disabled provider"
                    );
                    continue;
                }

                // Check for duplicate names
                if let Some((_, _existing)) =
                    registry.providers.iter().find(|(_, c)| c.name == config.name)
                {
                    // Duplicate names are OK if they come from the same ID
                    // (reload scenario). Different IDs with same name is an error.
                    let existing_id = registry
                        .providers
                        .iter()
                        .find(|(_, c)| c.name == config.name)
                        .map(|(id, _)| id.clone());
                    if existing_id.as_deref() != Some(&id) {
                        return Err(ProviderConfigError::DuplicateName {
                            name: config.name.clone(),
                            path1: path_str,
                            path2: format!(
                                "{}/{}.toml",
                                dir.display(),
                                existing_id.unwrap_or_default()
                            ),
                        });
                    }
                }

                tracing::info!(
                    provider = %id,
                    name = %config.name,
                    domains = %config.domain_patterns.len(),
                    pipelines = %config.deobfuscation_pipeline.len(),
                    "loaded provider config"
                );

                registry.providers.push((id, config));
            }
        }

        tracing::info!(
            count = registry.providers.len(),
            providers = ?registry.providers.iter().map(|(id, c)| format!("{} ({})", id, c.name)).collect::<Vec<_>>(),
            "provider registry loaded"
        );

        Ok(registry)
    }

    /// Load from a directory, but don't fail if the directory doesn't exist.
    /// Returns an empty registry in that case.
    pub fn load_from_dir_or_empty(dir: &Path) -> Self {
        match Self::load_from_dir(dir) {
            Ok(registry) => registry,
            Err(ProviderConfigError::Io { .. }) => {
                tracing::warn!(
                    path = %dir.display(),
                    "providers.d directory not found — no custom providers loaded"
                );
                Self::new()
            },
            Err(e) => {
                tracing::error!(error = %e, "failed to load provider configs — no custom providers loaded");
                Self::new()
            },
        }
    }

    /// Find the provider that matches a given hostname.
    ///
    /// Returns the provider ID and config for the first provider whose
    /// domain patterns match the hostname.
    pub fn find_provider_for_host(&self, host: &str) -> Option<(&str, &ProviderConfig)> {
        for (id, config) in &self.providers {
            if config.matches_domain(host) {
                return Some((id.as_str(), config));
            }
        }
        None
    }

    /// Get a provider by its ID (filename stem).
    pub fn get_by_id(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|(i, _)| i == id).map(|(_, c)| c)
    }

    /// Get all loaded providers.
    pub fn providers(&self) -> &[(String, ProviderConfig)] {
        &self.providers
    }

    /// Add a provider programmatically (for testing).
    pub fn add(&mut self, id: String, config: ProviderConfig) {
        self.providers.push((id, config));
    }

    /// Get all domain patterns across all providers (for the classifier).
    pub fn all_domain_patterns(&self) -> Vec<&DomainPattern> {
        self.providers.iter().flat_map(|(_, c)| c.domain_patterns.iter()).collect()
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_pattern_exact_match() {
        let dp = DomainPattern { pattern: "voe.sx".into(), kind: DomainMatchKind::Exact };
        assert!(dp.matches("voe.sx"));
        assert!(dp.matches("VOE.SX"));
        assert!(!dp.matches("sub.voe.sx"));
        assert!(!dp.matches("notvoe.sx"));
    }

    #[test]
    fn domain_pattern_suffix_match() {
        let dp = DomainPattern { pattern: "voe.sx".into(), kind: DomainMatchKind::Suffix };
        assert!(dp.matches("voe.sx"));
        assert!(dp.matches("sub.voe.sx"));
        assert!(dp.matches("VOE.SX"));
        assert!(!dp.matches("notvoe.sx"));
    }

    #[test]
    fn domain_pattern_regex_match() {
        let dp = DomainPattern { pattern: r"^voe\d*\.com$".into(), kind: DomainMatchKind::Regex };
        assert!(dp.matches("voe.com"));
        assert!(dp.matches("voe123.com"));
        assert!(!dp.matches("voe.org"));
    }

    #[test]
    fn provider_config_from_toml() {
        let toml = r#"
name = "TestProvider"
enabled = true

[[domain_patterns]]
pattern = "test.com"
kind = "exact"

[[domain_patterns]]
pattern = "test.org"
kind = "suffix"

[[deobfuscation_pipeline]]
name = "method8"
extraction = { method = "script_json_tag" }
steps = ["rot13", "base64_decode", { char_shift = { amount = 3 } }, "reverse", "base64_decode"]

[[deobfuscation_pipeline.url_extraction]]
json_key = { key = "mp4", priority = 1, append_rq_token = false }
"#;
        let config = ProviderConfig::from_toml_str(toml, "test.toml").unwrap();
        assert_eq!(config.name, "TestProvider");
        assert!(config.enabled);
        assert_eq!(config.domain_patterns.len(), 2);
        assert_eq!(config.deobfuscation_pipeline.len(), 1);
    }

    #[test]
    fn provider_config_validate_missing_name() {
        let config = ProviderConfig {
            name: String::new(),
            domain_patterns: vec![DomainPattern {
                pattern: "test.com".into(),
                kind: DomainMatchKind::Exact,
            }],
            ..Default::default()
        };
        // We can't easily create a default ProviderConfig due to HashMap,
        // so let's test the validation directly.
        let err = config.validate("test.toml");
        assert!(err.is_err());
        match err.unwrap_err() {
            ProviderConfigError::MissingField { field, .. } => assert_eq!(field, "name"),
            other => panic!("Expected MissingField, got {:?}", other),
        }
    }

    #[test]
    fn provider_config_validate_missing_domains() {
        let config =
            ProviderConfig { name: "Test".into(), domain_patterns: vec![], ..Default::default() };
        let err = config.validate("test.toml");
        assert!(err.is_err());
        match err.unwrap_err() {
            ProviderConfigError::MissingField { field, .. } => assert_eq!(field, "domain_patterns"),
            other => panic!("Expected MissingField, got {:?}", other),
        }
    }

    #[test]
    fn provider_config_validate_invalid_regex() {
        let config = ProviderConfig {
            name: "Test".into(),
            domain_patterns: vec![DomainPattern {
                pattern: "[invalid".into(),
                kind: DomainMatchKind::Regex,
            }],
            ..Default::default()
        };
        let err = config.validate("test.toml");
        assert!(err.is_err());
        match err.unwrap_err() {
            ProviderConfigError::InvalidRegex { .. } => {},
            other => panic!("Expected InvalidRegex, got {:?}", other),
        }
    }

    #[test]
    fn provider_registry_find_provider() {
        let mut registry = ProviderRegistry::new();
        let config = ProviderConfig {
            name: "Voe".into(),
            domain_patterns: vec![DomainPattern {
                pattern: "voe.sx".into(),
                kind: DomainMatchKind::Suffix,
            }],
            ..default_provider_config()
        };
        registry.add("voe".into(), config);

        assert!(registry.find_provider_for_host("voe.sx").is_some());
        assert!(registry.find_provider_for_host("sub.voe.sx").is_some());
        assert!(registry.find_provider_for_host("youtube.com").is_none());
    }

    #[test]
    fn provider_registry_get_by_id() {
        let mut registry = ProviderRegistry::new();
        let config = ProviderConfig {
            name: "Voe".into(),
            domain_patterns: vec![DomainPattern {
                pattern: "voe.sx".into(),
                kind: DomainMatchKind::Suffix,
            }],
            ..default_provider_config()
        };
        registry.add("voe".into(), config);

        assert!(registry.get_by_id("voe").is_some());
        assert!(registry.get_by_id("doodstream").is_none());
    }

    /// Helper to create a minimal valid ProviderConfig for tests.
    fn default_provider_config() -> ProviderConfig {
        ProviderConfig {
            name: String::new(),
            enabled: true,
            domain_patterns: vec![],
            resolver_type: ResolverType::Custom,
            deobfuscation_pipeline: vec![],
            url_extraction: vec![],
            quality_preferences: vec![],
            cdn: CdnConfig::default(),
            forward_cookies: false,
            request_headers: HashMap::new(),
            timeout_secs: 15,
            bait_domains: vec![],
            bait_filenames: vec![],
            content_extraction: vec![],
            post_resolve: vec![],
            url_transforms: vec![],
        }
    }
}
