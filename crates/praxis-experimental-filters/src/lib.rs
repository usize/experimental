//! Experimental Praxis AI filters.
//!
//! This crate is discovered at build time by praxis-ai's
//! `[package.metadata.praxis-filters]` auto-discovery (see praxis-ai
//! `server/build.rs` and `praxis-ai-build-support`). The generated registration
//! code calls [`register_filters`], which is emitted by the
//! [`praxis_filter::export_filters!`] macro invoked below.
//!
//! Shipped filters:
//!
//! - `token_ceiling`: interim per-key token spend ceilings over a fixed window (Standalone AI Gateway MVP epic,
//!   praxis-proxy/ai#758).
//! - `experimental_placeholder`: a no-op filter proving end-to-end discovery and registration, retained until the
//!   remaining MVP filters land (`switchyard_route` in praxis-proxy/experimental#2; `api_key_auth` under ai#758 Track
//!   B).

mod placeholder;
mod token_ceiling;

praxis_filter::export_filters! {
    http "experimental_placeholder" => placeholder::PlaceholderFilter::from_config,
    http "token_ceiling" => token_ceiling::TokenCeilingFilter::from_config,
}
