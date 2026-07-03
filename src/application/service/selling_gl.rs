//! Outbound GL-posting port (hand-authored, user-owned).
//!
//! This is the selling side of the GL-posting contract (`docs/erp/gl-posting-contract.md`).
//! Selling **emits** a balanced `AccountingPostEnvelope`; a delivery adapter (in the composing
//! service, or the seam test) maps it into `backbone-accounting`'s inbound port and returns an
//! ack. Selling never imports `backbone-accounting` — the only thing crossing the boundary is
//! this serializable envelope, so there is **zero horizontal Cargo edge** and the envelope is
//! the versioned contract, not a shared Rust type.
//!
//! The envelope mirrors the contract shape exactly (idempotency_key, company/branch, source_*,
//! posting_date, currency, posting_type, balanced lines) so an adapter is a pure field mapping.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One debit/credit line of an emitted posting. Exactly one of `debit`/`credit` is > 0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlPostLine {
    pub account_id: Uuid,
    pub debit: Decimal,
    pub credit: Decimal,
    /// "customer" | "supplier" | "employee" — required by accounting iff the account is AR/AP.
    pub party_type: Option<String>,
    pub party_id: Option<Uuid>,
    pub description: Option<String>,
}

impl GlPostLine {
    pub fn debit(account_id: Uuid, amount: Decimal) -> Self {
        Self { account_id, debit: amount, credit: Decimal::ZERO, party_type: None, party_id: None, description: None }
    }
    pub fn credit(account_id: Uuid, amount: Decimal) -> Self {
        Self { account_id, debit: Decimal::ZERO, credit: amount, party_type: None, party_id: None, description: None }
    }
    pub fn with_party(mut self, party_type: &str, party_id: Uuid) -> Self {
        self.party_type = Some(party_type.to_string());
        self.party_id = Some(party_id);
        self
    }
    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
}

/// A balanced posting request emitted by selling — the contract envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountingPostEnvelope {
    /// Producer-stable dedupe key (here: the invoice id). NOTE: accounting's actual dedupe key is
    /// `(company_id, source_type, source_id, posting_type)` via a partial unique index — an adapter
    /// MUST set `source_id` to this same identity (selling sets `source_id = invoice_id`). This
    /// field mirrors that identity for producers/buses that dedupe on an explicit key; it is
    /// redundant with `source_id` for the accounting seam, not the primary arbiter.
    pub idempotency_key: String,
    pub company_id: Uuid,
    pub branch_id: Option<Uuid>,
    /// Posting source discriminator (selling emits "order").
    pub source_type: String,
    /// The producer document id (the invoice id) — opaque to accounting.
    pub source_id: Uuid,
    pub source_reference: Option<String>,
    pub posting_date: chrono::NaiveDate,
    pub currency: String,
    /// "original" | "reversal".
    pub posting_type: String,
    pub description: Option<String>,
    pub lines: Vec<GlPostLine>,
}

impl AccountingPostEnvelope {
    /// Σdebit and Σcredit. The envelope is balanced iff they are equal.
    pub fn totals(&self) -> (Decimal, Decimal) {
        (
            self.lines.iter().map(|l| l.debit).sum(),
            self.lines.iter().map(|l| l.credit).sum(),
        )
    }
    pub fn is_balanced(&self) -> bool {
        let (d, c) = self.totals();
        d == c && !self.lines.is_empty()
    }
}

/// Acknowledgement returned by the GL after a successful post.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlPostAck {
    pub post_id: Uuid,
    pub journal_id: Uuid,
    /// True when accounting returned an already-posted entry (idempotent replay).
    pub idempotent_reuse: bool,
}

/// Rejection returned by the GL (validation failure). `code` is the stable contract error string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlPostRejected {
    pub code: String,
    pub message: String,
}

/// The delivery seam. A composing service implements this over `backbone-accounting`'s
/// `PostingService` (mapping envelope → PostingRequest); the seam test uses the same adapter.
/// Fire-and-return: selling reconciles its invoice from the ack/rejection.
#[async_trait::async_trait]
pub trait GlPostSink: Send + Sync {
    async fn post(&self, envelope: &AccountingPostEnvelope) -> Result<GlPostAck, GlPostRejected>;
}
