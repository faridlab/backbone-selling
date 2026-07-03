//! Reference CONSUMER extension (hand-authored, user-owned) — the second half of
//! extension-contract §5.
//!
//! This stands in for a downstream module/service (e.g. a credit-control app) that extends selling
//! WITHOUT modifying it: it subscribes to the selling domain event `SalesOrderConfirmed` and adds
//! its own business rule (flag orders whose total breaches a customer credit limit). It touches no
//! generated code — it only implements the public `SellingEventSink` trait — so it **survives a
//! regeneration of both modules**. `tests/extension_contract.rs` drives it and the regen round-trip
//! script (`docs/…`) proves the survival. In a real deployment this type lives in the consuming
//! service's own crate; here it lives in a `*_custom.rs` sibling to demonstrate the contract in-repo.

use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use uuid::Uuid;

use super::selling_events::{SellingEvent, SellingEventSink};

/// A credit-limit breach the consumer detected (its own domain concept, not selling's).
#[derive(Debug, Clone, PartialEq)]
pub struct CreditBreach {
    pub order_id: Uuid,
    pub customer_id: Uuid,
    pub grand_total: Decimal,
    pub limit: Decimal,
}

/// Consumer rule: watches `SalesOrderConfirmed` and records a breach when the order total exceeds
/// the configured credit limit. Pure in-memory (a real consumer would persist / alert).
pub struct CreditWatchConsumer {
    limit: Decimal,
    breaches: Arc<Mutex<Vec<CreditBreach>>>,
}

impl CreditWatchConsumer {
    pub fn new(limit: Decimal) -> Self {
        Self { limit, breaches: Arc::new(Mutex::new(Vec::new())) }
    }
    /// Shared handle to inspect what the rule recorded (for tests / the consuming app).
    pub fn breaches(&self) -> Arc<Mutex<Vec<CreditBreach>>> {
        self.breaches.clone()
    }
}

impl SellingEventSink for CreditWatchConsumer {
    fn publish(&self, event: SellingEvent) {
        if let SellingEvent::SalesOrderConfirmed(e) = event {
            if e.grand_total > self.limit {
                self.breaches.lock().unwrap().push(CreditBreach {
                    order_id: e.order_id,
                    customer_id: e.customer_id,
                    grand_total: e.grand_total,
                    limit: self.limit,
                });
            }
        }
    }
}
