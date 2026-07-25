//! Outbound GL-posting port (hand-authored, user-owned) — re-export of the shared contract.
//!
//! The GL-posting wire types (`AccountingPostEnvelope`, `GlPostLine`, `GlPostAck`, `GlPostRejected`)
//! and the `GlPostSink` port now live in the shared `backbone-gl-posting` crate (backbone-framework
//! v2.7.5) — the single source for all producers (phase 2). This file re-exports them under selling's
//! existing paths so selling's write service, tests, and `application::service::*` resolve unchanged.
//! Selling emits a balanced `AccountingPostEnvelope` (e.g. `Dr A/R · Cr Revenue` on invoice); a delivery
//! adapter maps it into accounting's `PostingRequest`. Zero normal Cargo edge into backbone-accounting.

pub use backbone_gl_posting::{
    AccountingPostEnvelope, GlPostAck, GlPostLine, GlPostRejected, GlPostSink,
};
