//! Per-key token spend ceilings over a fixed window.
//!
//! An explicitly **interim** filter for the Standalone AI Gateway MVP
//! (praxis-proxy/ai#758, success criterion 6): the operator names budget
//! keys in the config and caps how many tokens each may consume per
//! window; a key that has exhausted its ceiling receives `429` with
//! `Retry-After` until its window resets.
//!
//! # How it accounts
//!
//! Accounting is **post-hoc**: nothing is estimated or reserved at
//! admission. When a response completes, the provider-reported total that
//! the upstream `token_count` filter published to `filter_metadata` as
//! `token.total` is charged against the request's budget key. Admission
//! then denies a request only once its key's *committed* usage has reached
//! the ceiling. Consequences, all documented and accepted for the
//! single-user image:
//!
//! - A key can overshoot its ceiling by whatever is in flight, plus the final admitted request's own usage.
//! - State is in-memory and per-instance; replicas do not share budgets.
//! - Responses whose usage cannot be parsed (no `token.total`, e.g. the `token_count` capture limit overflowed) charge
//!   nothing.
//!
//! # Pipeline placement
//!
//! Declare this filter **before** `token_count` in the filter list:
//! response hooks run in reverse declared order, so `token_count` must
//! parse the response body (writing `token.total`) before this filter's
//! end-of-stream hook charges it — the same ordering contract
//! `token_usage_headers` relies on.
//!
//! # Relationship to the production design
//!
//! Production token rate limiting is owned by epic praxis-proxy/ai#121;
//! its first milestone landed upstream as the feature-gated
//! `token_rate_limit` filter (praxis-proxy/ai#796): reservation-based
//! admission, sliding-window/token-bucket algorithms, optional Valkey
//! shared state — but one shared budget per header-matched rule, with
//! identity/per-key quota deferred (praxis-proxy/grid#101). This filter is
//! the deliberately-small per-key answer until that lands, and keeps a
//! distinct name, config surface, and semantics so the two are never
//! confused. See `docs/token-ceiling.md` in this repository.

mod config;
mod ledger;

use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{
    BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, Rejection, parse_filter_config,
};

use self::{
    config::{KeySource, MAX_KEY_LENGTH, MissingKeyPolicy, TokenCeilingConfig},
    ledger::{Admission, CeilingLedger, Charge},
};

/// Metadata key stashing the request's resolved budget key, read back by
/// the end-of-stream hook to charge the right budget. Namespaced under
/// this filter's own name per `filter_metadata` convention.
const META_CEILING_KEY: &str = "token_ceiling.key";

/// Metadata key the upstream `token_count` filter writes the
/// provider-reported total token count to.
const META_TOKEN_TOTAL: &str = "token.total";

/// Bound on distinct budget keys tracked at once, so client-chosen keys
/// (the `header` source) cannot grow memory without limit. When full, new
/// keys fail closed with `503` rather than silently bypassing ceilings.
const MAX_TRACKED_KEYS: usize = 10_000;

/// Rate limit response header carrying the configured token ceiling. Uses
/// the `-Tokens` suffix convention from praxis-proxy/ai#124, matching the
/// upstream `token_rate_limit` filter, so clients read one header shape
/// regardless of which limiter rejected them.
const HEADER_RATELIMIT_LIMIT_TOKENS: &str = "X-RateLimit-Limit-Tokens";

/// Rate limit response header: tokens remaining (always `0`; the header is
/// only emitted on 429).
const HEADER_RATELIMIT_REMAINING_TOKENS: &str = "X-RateLimit-Remaining-Tokens";

/// Rate limit response header: seconds until the key's window resets.
const HEADER_RATELIMIT_RESET: &str = "X-RateLimit-Reset-Tokens";

/// Per-key fixed-window token spend ceilings, charged post-hoc from
/// `token.total` metadata.
///
/// # YAML configuration
///
/// ```yaml
/// filter: token_ceiling
/// window: 1h                # fixed window: 250ms | 30s | 5m | 1h forms
/// key:                      # where the budget key comes from; default:
///   metadata: identity.user #   the identity.user filter_metadata key
/// # key:
/// #   header: x-app-id      # or a request header (client-controlled!)
/// missing_key: allow        # allow (default) | reject keyless requests
/// ceilings:                 # per-key ceilings, tokens per window
///   alice: 200000
///   bob: 50000
/// default_ceiling: 10000    # optional: cap for keys not listed above;
///                           #   omit to leave unlisted keys un-capped
/// ```
#[derive(Debug)]
pub(crate) struct TokenCeilingFilter {
    /// Where the per-request budget key comes from.
    key_source: KeySource,

    /// Policy for requests whose key cannot be resolved.
    missing_key: MissingKeyPolicy,

    /// Per-key ceilings from the `ceilings:` map.
    ceilings: BTreeMap<String, u64>,

    /// Ceiling for keys not listed in `ceilings`, if configured.
    default_ceiling: Option<u64>,

    /// Per-key fixed-window usage state.
    ledger: CeilingLedger,

    /// Monotonic clock reference; all ledger timestamps are offsets from
    /// this.
    epoch: Instant,
}

/// The request-phase decision, computed apart from the filter context so
/// the interesting logic stays unit-testable (external filter crates
/// cannot construct an `HttpFilterContext`).
#[derive(Debug)]
enum RequestDecision {
    /// No ceiling applies (keyless and allowed, or no ceiling configured
    /// for this key): continue untracked.
    Untracked,

    /// Admitted under this key's ceiling: stash the key so the
    /// end-of-stream hook can charge it.
    Tracked(String),

    /// Reject the request with this action.
    Rejected(FilterAction),
}

impl TokenCeilingFilter {
    /// Builds a [`TokenCeilingFilter`] from filter configuration.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML does not deserialize or fails
    /// validation (see `config::validate`): zero/malformed window, no
    /// ceilings configured, a zero ceiling, or a bad key source.
    pub(crate) fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: TokenCeilingConfig = parse_filter_config("token_ceiling", config)?;
        let (window_ms, key_source) =
            config::validate(&cfg).map_err(|error| FilterError::from(format!("token_ceiling: {error}")))?;
        Ok(Box::new(Self {
            key_source,
            missing_key: cfg.missing_key,
            ceilings: cfg.ceilings,
            default_ceiling: cfg.default_ceiling,
            ledger: CeilingLedger::new(window_ms, MAX_TRACKED_KEYS),
            epoch: Instant::now(),
        }))
    }

    /// Milliseconds elapsed since this filter's epoch.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The ceiling applying to `key`: its `ceilings:` entry, else the
    /// `default_ceiling`, else none.
    fn ceiling_for(&self, key: &str) -> Option<u64> {
        self.ceilings.get(key).copied().or(self.default_ceiling)
    }

    /// Decides what to do with a request whose resolved key is `key`.
    fn decide(&self, key: Option<String>, now_ms: u64) -> RequestDecision {
        let Some(key) = key else {
            return match self.missing_key {
                MissingKeyPolicy::Allow => RequestDecision::Untracked,
                MissingKeyPolicy::Reject => RequestDecision::Rejected(missing_key_rejection()),
            };
        };
        let Some(ceiling) = self.ceiling_for(&key) else {
            return RequestDecision::Untracked;
        };
        match self.ledger.admit(&key, ceiling, now_ms) {
            Admission::Admit => RequestDecision::Tracked(key),
            Admission::Deny { retry_after_ms } => {
                tracing::info!(key = %key, ceiling, "token_ceiling: ceiling exhausted, rejecting request (429)");
                RequestDecision::Rejected(ceiling_exceeded(ceiling, retry_after_ms))
            },
            Admission::AtCapacity => {
                tracing::warn!(key = %key, "token_ceiling: key ledger at capacity, failing closed (503)");
                RequestDecision::Rejected(FilterAction::Reject(Rejection::status(503)))
            },
        }
    }

    /// Charges `key` with the response's reported usage, if parseable.
    fn settle(&self, key: &str, reported_total: Option<&str>, now_ms: u64) {
        let Some(tokens) = reported_total.and_then(|value| value.parse::<u64>().ok()) else {
            tracing::debug!(key = %key, "token_ceiling: no token.total at end of stream, nothing charged");
            return;
        };
        if self.ledger.charge(key, tokens, now_ms) == Charge::Dropped {
            tracing::warn!(key = %key, tokens, "token_ceiling: key ledger at capacity, charge dropped");
        }
    }
}

/// Resolves the request's budget key from the configured source.
///
/// Values are trimmed; an empty or over-long ([`MAX_KEY_LENGTH`]) value
/// counts as missing rather than being truncated, so two distinct long
/// keys can never collide into one budget.
fn resolve_key(source: &KeySource, headers: &http::HeaderMap, metadata: &HashMap<String, String>) -> Option<String> {
    let raw = match source {
        KeySource::Metadata(name) => metadata.get(name).map(String::as_str),
        KeySource::Header(name) => headers.get(name.as_str()).and_then(|value| value.to_str().ok()),
    }?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_KEY_LENGTH {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Builds the 429 rejection for an exhausted ceiling, carrying the
/// token-denominated rate limit headers (see [`HEADER_RATELIMIT_LIMIT_TOKENS`]).
fn ceiling_exceeded(ceiling: u64, retry_after_ms: u64) -> FilterAction {
    let retry_secs = (retry_after_ms.saturating_add(999) / 1000).max(1);
    FilterAction::Reject(
        Rejection::status(429)
            .with_header("Retry-After", retry_secs.to_string())
            .with_header(HEADER_RATELIMIT_LIMIT_TOKENS, ceiling.to_string())
            .with_header(HEADER_RATELIMIT_REMAINING_TOKENS, "0")
            .with_header(HEADER_RATELIMIT_RESET, retry_secs.to_string()),
    )
}

/// Builds the 403 rejection for a keyless request under
/// `missing_key: reject`.
fn missing_key_rejection() -> FilterAction {
    FilterAction::Reject(Rejection::status(403))
}

#[async_trait]
impl HttpFilter for TokenCeilingFilter {
    fn name(&self) -> &'static str {
        "token_ceiling"
    }

    fn response_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn response_body_mode(&self) -> BodyMode {
        BodyMode::Stream
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let key = resolve_key(&self.key_source, &ctx.request.headers, &ctx.filter_metadata);
        match self.decide(key, self.now_ms()) {
            RequestDecision::Untracked => Ok(FilterAction::Continue),
            RequestDecision::Tracked(key) => {
                ctx.set_metadata(META_CEILING_KEY, key);
                Ok(FilterAction::Continue)
            },
            RequestDecision::Rejected(action) => Ok(action),
        }
    }

    fn on_response_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if end_of_stream && let Some(key) = ctx.get_metadata(META_CEILING_KEY).map(str::to_owned) {
            self.settle(&key, ctx.get_metadata(META_TOKEN_TOTAL), self.now_ms());
            ctx.filter_metadata.remove(META_CEILING_KEY);
        }
        Ok(FilterAction::Continue)
    }
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
    clippy::indexing_slicing,
    clippy::wildcard_enum_match_arm,
    reason = "unwrap/expect/panic/indexing and catch-all match arms are acceptable in tests"
)]
mod tests {
    use std::collections::HashMap;

    use praxis_filter::{FilterAction, FilterRegistry, Rejection};

    use super::{
        META_CEILING_KEY, RequestDecision, TokenCeilingFilter, ceiling_exceeded, missing_key_rejection, resolve_key,
    };
    use crate::token_ceiling::config::KeySource;

    /// Builds the filter from YAML, panicking on config errors.
    fn filter(yaml: &str) -> Box<dyn praxis_filter::HttpFilter> {
        let value = serde_yaml::from_str(yaml).expect("test YAML should parse");
        TokenCeilingFilter::from_config(&value).expect("test config should build")
    }

    /// Builds the concrete filter type from YAML for decision-level tests.
    fn concrete(yaml: &str) -> TokenCeilingFilter {
        let value = serde_yaml::from_str(yaml).expect("test YAML should parse");
        let cfg = praxis_filter::parse_filter_config("token_ceiling", &value).expect("config should deserialize");
        let (window_ms, key_source) = super::config::validate(&cfg).expect("config should validate");
        TokenCeilingFilter {
            key_source,
            missing_key: cfg.missing_key,
            ceilings: cfg.ceilings,
            default_ceiling: cfg.default_ceiling,
            ledger: super::CeilingLedger::new(window_ms, super::MAX_TRACKED_KEYS),
            epoch: std::time::Instant::now(),
        }
    }

    /// Unwraps a [`RequestDecision::Rejected`] into its [`Rejection`].
    fn rejection(decision: RequestDecision) -> Rejection {
        match decision {
            RequestDecision::Rejected(FilterAction::Reject(rejection)) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn name_and_registration_match() {
        let built = filter("window: 1h\ndefault_ceiling: 10\n");
        assert_eq!(built.name(), "token_ceiling", "name must match the registration");
        let mut registry = FilterRegistry::with_builtins();
        crate::register_filters(&mut registry);
        assert!(
            registry.available_filters().contains(&"token_ceiling"),
            "token_ceiling must be registered"
        );
    }

    #[test]
    fn from_config_rejects_invalid_configuration() {
        let value = serde_yaml::from_str("window: 1h\n").expect("YAML should parse");
        let Err(error) = TokenCeilingFilter::from_config(&value) else {
            panic!("a config without ceilings must be rejected")
        };
        assert!(error.to_string().contains("token_ceiling:"), "got: {error}");
    }

    #[test]
    fn streams_the_response_body_read_only() {
        let built = filter("window: 1h\ndefault_ceiling: 10\n");
        assert!(matches!(
            built.response_body_access(),
            praxis_filter::BodyAccess::ReadOnly
        ));
        assert!(matches!(built.response_body_mode(), praxis_filter::BodyMode::Stream));
    }

    #[test]
    fn keyless_requests_pass_untracked_by_default() {
        let ceiling_filter = concrete("window: 1h\ndefault_ceiling: 10\n");
        assert!(matches!(ceiling_filter.decide(None, 0), RequestDecision::Untracked));
    }

    #[test]
    fn keyless_requests_are_rejected_under_reject_policy() {
        let ceiling_filter = concrete("window: 1h\nmissing_key: reject\ndefault_ceiling: 10\n");
        let denied = rejection(ceiling_filter.decide(None, 0));
        assert_eq!(denied.status, 403);
    }

    #[test]
    fn unlisted_keys_pass_untracked_without_a_default_ceiling() {
        let ceiling_filter = concrete("window: 1h\nceilings:\n  alice: 10\n");
        assert!(matches!(
            ceiling_filter.decide(Some("bob".to_owned()), 0),
            RequestDecision::Untracked
        ));
    }

    #[test]
    fn admitted_requests_are_tracked_under_their_key() {
        let ceiling_filter = concrete("window: 1h\nceilings:\n  alice: 10\n");
        match ceiling_filter.decide(Some("alice".to_owned()), 0) {
            RequestDecision::Tracked(key) => assert_eq!(key, "alice"),
            other => panic!("expected Tracked, got {other:?}"),
        }
    }

    #[test]
    fn exhausted_ceiling_yields_429_with_rate_limit_headers() {
        let ceiling_filter = concrete("window: 1m\nceilings:\n  alice: 100\n");
        ceiling_filter.settle("alice", Some("100"), 0);
        let denied = rejection(ceiling_filter.decide(Some("alice".to_owned()), 30_000));
        assert_eq!(denied.status, 429);
        let headers: HashMap<&str, &str> = denied
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(headers["Retry-After"], "30");
        assert_eq!(headers["X-RateLimit-Limit-Tokens"], "100");
        assert_eq!(headers["X-RateLimit-Remaining-Tokens"], "0");
        assert_eq!(headers["X-RateLimit-Reset-Tokens"], "30");
    }

    #[test]
    fn settle_ignores_missing_and_malformed_totals() {
        let ceiling_filter = concrete("window: 1h\nceilings:\n  alice: 10\n");
        ceiling_filter.settle("alice", None, 0);
        ceiling_filter.settle("alice", Some("not-a-number"), 0);
        assert!(matches!(
            ceiling_filter.decide(Some("alice".to_owned()), 1),
            RequestDecision::Tracked(_)
        ));
    }

    #[test]
    fn a_new_window_admits_an_exhausted_key_again() {
        let ceiling_filter = concrete("window: 1m\nceilings:\n  alice: 10\n");
        ceiling_filter.settle("alice", Some("10"), 0);
        assert!(matches!(
            ceiling_filter.decide(Some("alice".to_owned()), 0),
            RequestDecision::Rejected(_)
        ));
        assert!(matches!(
            ceiling_filter.decide(Some("alice".to_owned()), 60_000),
            RequestDecision::Tracked(_)
        ));
    }

    #[test]
    fn default_ceiling_applies_to_unlisted_keys() {
        let ceiling_filter = concrete("window: 1h\nceilings:\n  alice: 100\ndefault_ceiling: 10\n");
        ceiling_filter.settle("bob", Some("10"), 0);
        assert!(matches!(
            ceiling_filter.decide(Some("bob".to_owned()), 0),
            RequestDecision::Rejected(_)
        ));
        // alice's own, higher ceiling still applies.
        ceiling_filter.settle("alice", Some("10"), 0);
        assert!(matches!(
            ceiling_filter.decide(Some("alice".to_owned()), 0),
            RequestDecision::Tracked(_)
        ));
    }

    #[test]
    fn resolve_key_reads_the_configured_header_case_insensitively() {
        let mut headers = http::HeaderMap::new();
        headers.insert("X-App-Id", "  alpha  ".parse().expect("valid header value"));
        let source = KeySource::Header("x-app-id".to_owned());
        assert_eq!(
            resolve_key(&source, &headers, &HashMap::new()),
            Some("alpha".to_owned()),
            "header lookup must be case-insensitive and trimmed"
        );
        assert_eq!(
            resolve_key(&KeySource::Header("x-other".to_owned()), &headers, &HashMap::new()),
            None
        );
    }

    #[test]
    fn resolve_key_reads_the_configured_metadata_key() {
        let headers = http::HeaderMap::new();
        let mut metadata = HashMap::new();
        metadata.insert("identity.user".to_owned(), "alice".to_owned());
        let source = KeySource::Metadata("identity.user".to_owned());
        assert_eq!(resolve_key(&source, &headers, &metadata), Some("alice".to_owned()));
        assert_eq!(
            resolve_key(&KeySource::Metadata("identity.group".to_owned()), &headers, &metadata),
            None
        );
    }

    #[test]
    fn resolve_key_treats_empty_and_overlong_values_as_missing() {
        let headers = http::HeaderMap::new();
        let mut metadata = HashMap::new();
        metadata.insert("identity.user".to_owned(), "   ".to_owned());
        let source = KeySource::Metadata("identity.user".to_owned());
        assert_eq!(resolve_key(&source, &headers, &metadata), None, "blank is missing");
        metadata.insert("identity.user".to_owned(), "k".repeat(257));
        assert_eq!(resolve_key(&source, &headers, &metadata), None, "over-long is missing");
    }

    #[test]
    fn ceiling_exceeded_rounds_retry_after_up_and_never_below_one() {
        let sub_second = rejection(RequestDecision::Rejected(ceiling_exceeded(10, 1)));
        let headers: HashMap<&str, &str> = sub_second
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(headers["Retry-After"], "1", "sub-second waits round up to 1");
        let zero = rejection(RequestDecision::Rejected(ceiling_exceeded(10, 0)));
        assert_eq!(zero.headers[0].1, "1", "a zero wait still advises 1 second");
    }

    #[test]
    fn missing_key_rejection_is_a_bare_403() {
        let denied = rejection(RequestDecision::Rejected(missing_key_rejection()));
        assert_eq!(denied.status, 403);
        assert!(denied.headers.is_empty());
        assert!(denied.body.is_none());
    }

    #[test]
    fn stash_metadata_key_is_within_context_limits() {
        // filter_metadata silently drops keys over 64 bytes; the stash key
        // must never be one of them.
        assert!(META_CEILING_KEY.len() <= 64);
    }
}
