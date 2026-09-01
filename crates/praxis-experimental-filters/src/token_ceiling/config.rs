//! Deserialized YAML configuration for the `token_ceiling` filter.
//!
//! The surface is deliberately flat and deliberately *not* shaped like the
//! production token rate limiting design (epic praxis-proxy/ai#121 and the
//! upstream `token_rate_limit` filter from praxis-proxy/ai#796): no rules
//! list, no algorithm choice, no reservation estimate. A ceiling is one
//! number per key over one fixed window, so migrating off this interim
//! filter later never means untangling a config that impersonated the
//! production one.

use std::collections::BTreeMap;

use serde::Deserialize;

/// `filter_metadata` key consulted when `key:` is omitted. Matches the
/// identity key the withdrawn identity-metadata contract proposed for
/// `api_key_auth` and friends (see praxis-proxy/experimental commit
/// `a8aa13b`); once that contract is re-filed and accepted upstream, this
/// default should already line up.
pub(super) const DEFAULT_IDENTITY_METADATA_KEY: &str = "identity.user";

/// Bound on a resolved budget key's length in bytes. Matches the
/// `filter_metadata` value limit, so any key an identity filter could have
/// published is representable, and longer client-supplied header values
/// are treated as missing rather than truncated (truncation could collide
/// two distinct keys into one budget).
pub(super) const MAX_KEY_LENGTH: usize = 256;

/// Deserialized YAML config for the `token_ceiling` filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TokenCeilingConfig {
    /// Fixed window duration, e.g. `"30s"`, `"5m"`, `"1h"`. Each key's
    /// window starts at its first charged response and resets wholesale
    /// once it elapses.
    pub(super) window: String,

    /// Where the per-request budget key comes from. Defaults to the
    /// `identity.user` metadata key.
    #[serde(default)]
    pub(super) key: KeySourceConfig,

    /// What to do with a request whose key cannot be resolved.
    #[serde(default)]
    pub(super) missing_key: MissingKeyPolicy,

    /// Per-key token ceilings: the maximum `token.total` a key may accrue
    /// within one window before further requests receive 429.
    #[serde(default)]
    pub(super) ceilings: BTreeMap<String, u64>,

    /// Ceiling applied to keys not listed in `ceilings`. `None` (omitted)
    /// leaves unlisted keys un-capped and untracked — semantically
    /// distinct from any number, hence an `Option` rather than a default.
    #[serde(default)]
    pub(super) default_ceiling: Option<u64>,
}

/// The `key:` block as written in YAML: at most one of `metadata`/`header`.
///
/// A pair of optional fields rather than a serde enum because the
/// workspace's YAML library maps externally-tagged enums to `!tag` syntax;
/// [`resolve_key_source`] enforces exactly-one and produces the
/// [`KeySource`] the filter actually runs on.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct KeySourceConfig {
    /// Read this `filter_metadata` key; see [`KeySource::Metadata`].
    #[serde(default)]
    pub(super) metadata: Option<String>,

    /// Read this request header's value; see [`KeySource::Header`].
    #[serde(default)]
    pub(super) header: Option<String>,
}

/// Where the per-request budget key comes from, resolved and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KeySource {
    /// Read this `filter_metadata` key, written by an identity-producing
    /// filter earlier in the pipeline (e.g. the planned `api_key_auth`).
    /// This is the trustworthy source: clients cannot write metadata.
    Metadata(String),

    /// Read this request header's value. **Client-controlled**: anyone who
    /// can reach the gateway can pick their own budget by picking their
    /// own header value. Fine for the single-user image and for demos;
    /// not an enforcement boundary against untrusted callers.
    Header(String),
}

/// Policy for a request whose budget key cannot be resolved (source
/// absent, empty after trimming, or longer than [`MAX_KEY_LENGTH`]).
#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum MissingKeyPolicy {
    /// Let keyless requests through, untracked and un-capped. The default:
    /// establishing identity is an authentication filter's job, and that
    /// filter — not this one — decides whether anonymous traffic passes.
    #[default]
    Allow,

    /// Reject keyless requests with `403 Forbidden`, for pipelines where
    /// every request is expected to carry a budget key and a missing one
    /// means misconfiguration rather than anonymity.
    Reject,
}

/// Validates the parsed configuration's business rules, resolving
/// `window` to milliseconds and the `key:` block to a [`KeySource`].
///
/// # Errors
///
/// Returns a message (without the filter-name prefix) if the window is
/// invalid or zero, no ceiling is configured at all, any ceiling is zero,
/// any `ceilings` key is empty or over-long, or the key source is
/// over-specified / names an invalid header / an empty metadata key.
pub(super) fn validate(config: &TokenCeilingConfig) -> Result<(u64, KeySource), String> {
    let window_ms = parse_duration_ms(&config.window)?;
    if window_ms == 0 {
        return Err("window must be greater than zero".to_owned());
    }
    if config.ceilings.is_empty() && config.default_ceiling.is_none() {
        return Err("configure at least one entry in ceilings, or default_ceiling".to_owned());
    }
    for (key, ceiling) in &config.ceilings {
        validate_ceiling_entry(key, *ceiling)?;
    }
    if config.default_ceiling == Some(0) {
        return Err("default_ceiling must be greater than zero".to_owned());
    }
    let key_source = resolve_key_source(&config.key)?;
    Ok((window_ms, key_source))
}

/// Validates one `ceilings:` entry.
///
/// # Errors
///
/// Returns a message if the key is empty or over-long, or the ceiling is
/// zero.
fn validate_ceiling_entry(key: &str, ceiling: u64) -> Result<(), String> {
    if key.is_empty() {
        return Err("ceilings keys must not be empty".to_owned());
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(format!("ceilings key '{key}' exceeds {MAX_KEY_LENGTH} bytes"));
    }
    if ceiling == 0 {
        return Err(format!("ceiling for '{key}' must be greater than zero"));
    }
    Ok(())
}

/// Resolves the written `key:` block into a validated [`KeySource`],
/// defaulting to [`DEFAULT_IDENTITY_METADATA_KEY`] metadata when the block
/// is omitted (or written empty).
///
/// # Errors
///
/// Returns a message if both sources are set, the metadata key is empty,
/// or the header name is invalid.
fn resolve_key_source(config: &KeySourceConfig) -> Result<KeySource, String> {
    match (&config.metadata, &config.header) {
        (Some(_), Some(_)) => Err("key: set exactly one of metadata or header".to_owned()),
        (Some(name), None) => {
            if name.is_empty() {
                return Err("key.metadata must not be empty".to_owned());
            }
            Ok(KeySource::Metadata(name.clone()))
        },
        (None, Some(name)) => {
            validate_header_name(name)?;
            Ok(KeySource::Header(name.clone()))
        },
        (None, None) => Ok(KeySource::Metadata(DEFAULT_IDENTITY_METADATA_KEY.to_owned())),
    }
}

/// Validates an HTTP header field name (an RFC 9110 token), so a typo like
/// `key: {header: "x app id"}` fails at startup instead of silently never
/// matching a request.
///
/// # Errors
///
/// Returns a message if the name is empty or contains a non-token byte.
fn validate_header_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("key.header must not be empty".to_owned());
    }
    let valid = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(format!("key.header '{name}' is not a valid header name"))
    }
}

/// Parses a `<number><unit>` duration (`ms`, `s`, `m`, `h`) into
/// milliseconds, matching the format the upstream `token_rate_limit`
/// filter accepts for its windows.
///
/// # Errors
///
/// Returns a message if the unit is missing/unknown, the number does not
/// parse, or the result overflows `u64`.
fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(rest) = value.strip_suffix("ms") {
        (rest, 1_u64)
    } else if let Some(rest) = value.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = value.strip_suffix('m') {
        (rest, 60_000)
    } else if let Some(rest) = value.strip_suffix('h') {
        (rest, 3_600_000)
    } else {
        return Err(format!("invalid window '{value}': expected <number><ms|s|m|h>"));
    };
    let count: u64 = number
        .trim()
        .parse()
        .map_err(|error| format!("invalid window '{value}': {error}"))?;
    count
        .checked_mul(multiplier)
        .ok_or_else(|| format!("window '{value}' overflows"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "unwrap/expect/panic are acceptable in tests"
)]
mod tests {
    use super::{KeySource, MissingKeyPolicy, TokenCeilingConfig, parse_duration_ms, validate};

    /// Parses YAML into a config, panicking on deserialization errors.
    fn parse(yaml: &str) -> TokenCeilingConfig {
        serde_yaml::from_str(yaml).expect("test YAML should deserialize")
    }

    #[test]
    fn minimal_config_uses_identity_metadata_defaults() {
        let cfg = parse("window: 1h\nceilings:\n  alice: 1000\n");
        assert_eq!(cfg.missing_key, MissingKeyPolicy::Allow);
        assert_eq!(cfg.default_ceiling, None);
        let (window_ms, key_source) = validate(&cfg).unwrap();
        assert_eq!(window_ms, 3_600_000);
        assert_eq!(key_source, KeySource::Metadata("identity.user".to_owned()));
    }

    #[test]
    fn header_key_source_and_reject_policy_parse() {
        let cfg = parse(
            "window: 30s\nkey:\n  header: x-app-id\nmissing_key: reject\nceilings:\n  alpha: 500\ndefault_ceiling: \
             100\n",
        );
        assert_eq!(cfg.missing_key, MissingKeyPolicy::Reject);
        assert_eq!(cfg.default_ceiling, Some(100));
        let (window_ms, key_source) = validate(&cfg).unwrap();
        assert_eq!(window_ms, 30_000);
        assert_eq!(key_source, KeySource::Header("x-app-id".to_owned()));
    }

    #[test]
    fn metadata_key_source_parses_explicitly() {
        let cfg = parse("window: 1m\nkey:\n  metadata: identity.group\ndefault_ceiling: 9\n");
        let (window_ms, key_source) = validate(&cfg).unwrap();
        assert_eq!(window_ms, 60_000);
        assert_eq!(key_source, KeySource::Metadata("identity.group".to_owned()));
    }

    #[test]
    fn over_specified_key_sources_are_rejected() {
        let both = parse("window: 1m\nkey:\n  metadata: identity.user\n  header: x-app-id\ndefault_ceiling: 9\n");
        assert!(validate(&both).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn an_empty_key_block_falls_back_to_the_default_source() {
        let empty_block = parse("window: 1m\nkey: {}\ndefault_ceiling: 9\n");
        let (_, key_source) = validate(&empty_block).unwrap();
        assert_eq!(key_source, KeySource::Metadata("identity.user".to_owned()));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // `capacity` is the ai#121 surface; it must not silently parse here.
        let result: Result<TokenCeilingConfig, _> = serde_yaml::from_str("window: 1h\ncapacity: 100\n");
        assert!(result.is_err(), "ai#121-style fields must be rejected");
    }

    #[test]
    fn validate_rejects_a_configuration_without_ceilings() {
        let cfg = parse("window: 1h\n");
        let error = validate(&cfg).unwrap_err();
        assert!(error.contains("at least one"), "got: {error}");
    }

    #[test]
    fn validate_rejects_zero_ceilings() {
        let zero_listed = parse("window: 1h\nceilings:\n  alice: 0\n");
        assert!(validate(&zero_listed).unwrap_err().contains("greater than zero"));
        let zero_default = parse("window: 1h\ndefault_ceiling: 0\n");
        assert!(validate(&zero_default).unwrap_err().contains("default_ceiling"));
    }

    #[test]
    fn validate_rejects_empty_and_overlong_ceiling_keys() {
        let empty_key = parse("window: 1h\nceilings:\n  \"\": 10\n");
        assert!(validate(&empty_key).unwrap_err().contains("must not be empty"));
        let long_key = "k".repeat(257);
        let overlong_key = parse(&format!("window: 1h\nceilings:\n  {long_key}: 10\n"));
        assert!(validate(&overlong_key).unwrap_err().contains("exceeds"));
    }

    #[test]
    fn validate_rejects_bad_key_sources() {
        let empty_metadata = parse("window: 1h\nkey:\n  metadata: \"\"\ndefault_ceiling: 5\n");
        assert!(validate(&empty_metadata).unwrap_err().contains("key.metadata"));
        let spaced_header = parse("window: 1h\nkey:\n  header: \"x app id\"\ndefault_ceiling: 5\n");
        assert!(
            validate(&spaced_header)
                .unwrap_err()
                .contains("not a valid header name")
        );
        let empty_header = parse("window: 1h\nkey:\n  header: \"\"\ndefault_ceiling: 5\n");
        assert!(validate(&empty_header).unwrap_err().contains("key.header"));
    }

    #[test]
    fn validate_rejects_zero_and_malformed_windows() {
        let zero_window = parse("window: 0s\ndefault_ceiling: 5\n");
        assert!(validate(&zero_window).unwrap_err().contains("greater than zero"));
        for bad in ["", "1d", "h", "1.5h", "-1s", "1"] {
            let result = parse_duration_ms(bad);
            assert!(result.is_err(), "window '{bad}' should be rejected");
        }
    }

    #[test]
    fn durations_parse_every_supported_unit() {
        assert_eq!(parse_duration_ms("250ms").unwrap(), 250);
        assert_eq!(parse_duration_ms("30s").unwrap(), 30_000);
        assert_eq!(parse_duration_ms("5m").unwrap(), 300_000);
        assert_eq!(parse_duration_ms(" 1h ").unwrap(), 3_600_000);
    }

    #[test]
    fn duration_overflow_is_an_error_not_a_wrap() {
        let result = parse_duration_ms(&format!("{}h", u64::MAX));
        assert!(result.unwrap_err().contains("overflow"));
    }
}
