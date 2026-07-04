//! Selling domain events — the public extension surface (hand-authored, user-owned).
//!
//! These are the SEMANTIC events a consuming module/service subscribes to (per the module brief
//! and extension-contract §5), distinct from the generated CRUD `Created/Updated/Deleted` events.
//! Selling publishes them through a `SellingEventSink`; a real deployment wires a bus adapter, a
//! consumer adds its own rule against them, and — critically — that consumer rule survives a
//! regeneration of both modules because it lives in `user_owned` / `*_custom.rs` files, never in
//! generated code. `tests/extension_contract.rs` demonstrates the round-trip.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A customer accepted a quotation (it is now convertible to a sales order).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotationAccepted {
    pub quotation_id: Uuid,
    pub company_id: Uuid,
    pub customer_id: Uuid,
}

/// A sales order was confirmed (the demand commitment). Carries the totals a consumer needs
/// (e.g. credit-limit evaluation, fulfillment planning) without a call back into selling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesOrderConfirmed {
    pub order_id: Uuid,
    pub company_id: Uuid,
    pub customer_id: Uuid,
    pub grand_total: Decimal,
    pub currency: String,
}

/// A sales invoice was created (issued) from an order or directly — before it posts to the GL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesInvoiceIssued {
    pub invoice_id: Uuid,
    pub sales_order_id: Option<Uuid>,
    pub company_id: Uuid,
    pub customer_id: Uuid,
    pub total: Decimal,
}

/// A sales invoice's revenue was posted to the GL (reconciled from the AccountingPost ack).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesInvoicePosted {
    pub invoice_id: Uuid,
    pub company_id: Uuid,
    pub journal_id: Uuid,
    pub post_id: Uuid,
    pub total: Decimal,
}

/// One line of a delivery request (what selling asks inventory to ship).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryRequestLine {
    pub item_id: Uuid,
    pub quantity: Decimal,
}

/// The cross-module request selling emits when a confirmed order is ready to ship. Serialized (the
/// wire contract) — a fulfillment/composition layer maps it into inventory's own `DeliveryRequested`
/// (adding the warehouse + GL accounts inventory owns), so selling stays ignorant of inventory's
/// internals. Zero shared Rust type, zero Cargo edge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryRequestEnvelope {
    pub order_id: Uuid,
    pub company_id: Uuid,
    pub customer_id: Uuid,
    pub currency: String,
    pub lines: Vec<DeliveryRequestLine>,
}

/// The selling domain-event union (discriminated) published on the module event bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum SellingEvent {
    QuotationAccepted(QuotationAccepted),
    SalesOrderConfirmed(SalesOrderConfirmed),
    SalesInvoiceIssued(SalesInvoiceIssued),
    SalesInvoicePosted(SalesInvoicePosted),
    DeliveryRequested(DeliveryRequestEnvelope),
}

/// Exported reference DTO for a sales order — the shape a consumer holds (per the brief), richer
/// than the generated `{id}` CRUD ref. Built by `SellingWriteService::sales_order_ref`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesOrderRef {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub company_id: Uuid,
    pub grand_total: Decimal,
    pub currency: String,
}

/// Exported reference DTO for a quotation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotationRef {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub company_id: Uuid,
    pub grand_total: Decimal,
    pub currency: String,
}

/// Sink for selling domain events (the event-bus seam). Fire-and-forget. A real adapter
/// (e.g. backbone-messaging) implements this; consumers add rules against it; tests record.
pub trait SellingEventSink: Send + Sync {
    fn publish(&self, event: SellingEvent);
}

/// Default sink — emits structured tracing events.
pub struct LoggingSink;

impl SellingEventSink for LoggingSink {
    fn publish(&self, event: SellingEvent) {
        tracing::info!(target: "selling.events", ?event, "selling domain event");
    }
}
