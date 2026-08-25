//! Quotation state-machine goldens (Odoo sale.order semantics, adapted).
//!
//! Every verb is a guarded single-statement flip whose WHERE clause IS the guard, so these tests
//! prove both halves: the happy-path transition AND the loud refusal (`invalid_transition`,
//! `quotation_ordered` — never a silent no-op). `ordered` is a one-way door: a confirmed order
//! must never be orphaned by resetting or cancelling its source quotation.
//!
//! Requires DATABASE_URL pointing at a Postgres with the selling schema migrated.

use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_events::{SellingEvent, SellingEventSink};
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewQuotation, SellingError, SellingWriteService,
};

#[derive(Default, Clone)]
struct RecordingSink {
    events: Arc<Mutex<Vec<SellingEvent>>>,
}
impl SellingEventSink for RecordingSink {
    fn publish(&self, e: SellingEvent) {
        self.events.lock().unwrap().push(e);
    }
}
impl RecordingSink {
    fn has<F: Fn(&SellingEvent) -> bool>(&self, f: F) -> bool {
        self.events.lock().unwrap().iter().any(f)
    }
}

fn d(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}
fn uq(p: &str) -> String {
    format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8])
}
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
fn line() -> NewLine {
    NewLine {
        invoice_policy: None,
        is_downpayment: None,
        item_id: Uuid::new_v4(),
        revenue_account_id: None,
        description: None,
        quantity: d("1"),
        unit_price: d("100000"),
        line_discount: Decimal::ZERO,
    }
}
async fn draft_quotation(w: &SellingWriteService, company: Uuid) -> Uuid {
    w.create_quotation(NewQuotation {
        opportunity_id: None,
        template_id: None,
        quotation_number: uq("QUO"),
        company_id: company,
        branch_id: None,
        customer_id: Uuid::new_v4(),
        quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        valid_until: None,
        currency: None,
        tax_rate: Decimal::ZERO,
        notes: None,
        lines: vec![line()],
    })
    .await
    .unwrap()
}
async fn status_of(pool: &PgPool, quotation: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status::text, status_reason FROM selling.quotations WHERE id=$1")
        .bind(quotation)
        .fetch_one(pool)
        .await
        .unwrap()
}

// QM-1: send moves draft → sent and emits QuotationSent.
#[tokio::test]
async fn send_moves_draft_to_sent() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let rec = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(rec.clone()));
    let q = draft_quotation(&w, company).await;

    w.send_quotation(q, company).await.unwrap();
    assert_eq!(status_of(&pool, q).await.0, "sent");
    assert!(rec.has(|e| matches!(e, SellingEvent::QuotationSent(e) if e.quotation_id == q)));
}

// QM-2: send refuses any state other than draft (a re-send is a loud 422, not a no-op).
#[tokio::test]
async fn send_refuses_non_draft() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());
    let q = draft_quotation(&w, company).await;
    w.send_quotation(q, company).await.unwrap();

    let e = w.send_quotation(q, company).await.unwrap_err();
    assert!(matches!(e, SellingError::InvalidTransition { ref verb, ref current }
        if verb == "send" && current == "sent"));
    assert_eq!(SellingError::InvalidTransition { verb: "send".into(), current: "sent".into() }.http_status(), 422);
}

// QM-3: reject moves sent → rejected and persists the optional reason.
#[tokio::test]
async fn reject_records_reason() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let rec = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(rec.clone()));
    let q = draft_quotation(&w, company).await;
    w.send_quotation(q, company).await.unwrap();

    w.reject_quotation(q, company, Some("customer chose a competitor".into())).await.unwrap();
    let (st, reason) = status_of(&pool, q).await;
    assert_eq!(st, "rejected");
    assert_eq!(reason.as_deref(), Some("customer chose a competitor"));
    assert!(rec.has(|e| matches!(e, SellingEvent::QuotationRejected(e) if e.quotation_id == q && e.reason.is_some())));
}

// QM-4: reject refuses a draft (nothing was sent, so nothing can be declined).
#[tokio::test]
async fn reject_refuses_draft() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());
    let q = draft_quotation(&w, company).await;

    let e = w.reject_quotation(q, company, None).await.unwrap_err();
    assert!(matches!(e, SellingError::InvalidTransition { ref verb, ref current }
        if verb == "reject" && current == "draft"));
}

// QM-5: cancel is the exit from draft/sent/accepted, recording the reason.
#[tokio::test]
async fn cancel_exits_from_draft_sent_accepted() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());

    // draft → cancelled
    let a = draft_quotation(&w, company).await;
    w.cancel_quotation(a, company, Some("withdrawn".into())).await.unwrap();
    assert_eq!(status_of(&pool, a).await, ("cancelled".into(), Some("withdrawn".into())));

    // sent → cancelled
    let b = draft_quotation(&w, company).await;
    w.send_quotation(b, company).await.unwrap();
    w.cancel_quotation(b, company, None).await.unwrap();
    assert_eq!(status_of(&pool, b).await.0, "cancelled");

    // accepted → cancelled
    let c = draft_quotation(&w, company).await;
    w.accept_quotation(c, company).await.unwrap();
    w.cancel_quotation(c, company, None).await.unwrap();
    assert_eq!(status_of(&pool, c).await.0, "cancelled");
}

// QM-6: cancel refuses an ordered quotation — an order was derived from it (the one-way door).
#[tokio::test]
async fn cancel_refuses_ordered() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());
    let q = draft_quotation(&w, company).await;
    w.accept_quotation(q, company).await.unwrap();
    w.convert_quotation_to_order(q, uq("SO")).await.unwrap();
    assert_eq!(status_of(&pool, q).await.0, "ordered");

    let e = w.cancel_quotation(q, company, None).await.unwrap_err();
    assert!(matches!(e, SellingError::QuotationOrdered(id) if id == q));
    assert_eq!(SellingError::QuotationOrdered(q).http_status(), 422);
    assert_eq!(status_of(&pool, q).await.0, "ordered", "a refused cancel leaves the state untouched");
}

// QM-7: re-draft returns sent/rejected/cancelled to draft and clears the recorded reason.
#[tokio::test]
async fn redraft_returns_editable_states_to_draft() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());

    // rejected → draft (reason cleared)
    let a = draft_quotation(&w, company).await;
    w.send_quotation(a, company).await.unwrap();
    w.reject_quotation(a, company, Some("too expensive".into())).await.unwrap();
    w.redraft_quotation(a, company).await.unwrap();
    assert_eq!(status_of(&pool, a).await, ("draft".into(), None));

    // cancelled → draft
    let b = draft_quotation(&w, company).await;
    w.cancel_quotation(b, company, Some("withdrawn".into())).await.unwrap();
    w.redraft_quotation(b, company).await.unwrap();
    assert_eq!(status_of(&pool, b).await.0, "draft");

    // sent → draft
    let c = draft_quotation(&w, company).await;
    w.send_quotation(c, company).await.unwrap();
    w.redraft_quotation(c, company).await.unwrap();
    assert_eq!(status_of(&pool, c).await.0, "draft");
}

// QM-8: re-draft refuses an ordered quotation (never from `ordered`).
#[tokio::test]
async fn redraft_refuses_ordered() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());
    let q = draft_quotation(&w, company).await;
    w.accept_quotation(q, company).await.unwrap();
    w.convert_quotation_to_order(q, uq("SO")).await.unwrap();

    let e = w.redraft_quotation(q, company).await.unwrap_err();
    assert!(matches!(e, SellingError::InvalidTransition { ref verb, ref current }
        if verb == "re-draft" && current == "ordered"));
}

// QM-9: the machine round-trips — draft → sent → rejected → draft → sent → accepted.
#[tokio::test]
async fn full_round_trip_to_accepted() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());
    let q = draft_quotation(&w, company).await;

    w.send_quotation(q, company).await.unwrap();
    w.reject_quotation(q, company, None).await.unwrap();
    w.redraft_quotation(q, company).await.unwrap();
    w.send_quotation(q, company).await.unwrap();
    w.accept_quotation(q, company).await.unwrap();
    assert_eq!(status_of(&pool, q).await.0, "accepted");
}

// QM-10: a wrong-tenant or unknown id is a 404 not-found, never a state-machine refusal — the
// guarded statement does not leak whether the id exists, and the post-refusal classification
// only runs after the refusal.
#[tokio::test]
async fn unknown_or_foreign_quotation_is_not_found() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let w = SellingWriteService::new(pool.clone());

    let stranger = draft_quotation(&w, Uuid::new_v4()).await; // another tenant's quotation
    for e in [
        w.send_quotation(stranger, company).await.unwrap_err(),
        w.accept_quotation(stranger, company).await.unwrap_err(),
        w.reject_quotation(stranger, company, None).await.unwrap_err(),
        w.cancel_quotation(stranger, company, None).await.unwrap_err(),
        w.redraft_quotation(stranger, company).await.unwrap_err(),
        w.send_quotation(Uuid::new_v4(), company).await.unwrap_err(),
    ] {
        assert!(matches!(e, SellingError::QuotationNotFound(_)));
        assert_eq!(SellingError::QuotationNotFound(Uuid::new_v4()).http_status(), 404);
    }
}

// QM-11: every machine verb emits its event exactly once per successful transition.
#[tokio::test]
async fn events_fire_once_per_transition() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let rec = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(rec.clone()));
    let q = draft_quotation(&w, company).await;

    w.send_quotation(q, company).await.unwrap();
    w.reject_quotation(q, company, None).await.unwrap();
    w.redraft_quotation(q, company).await.unwrap();
    w.cancel_quotation(q, company, None).await.unwrap();

    let count = |f: &dyn Fn(&SellingEvent) -> bool| rec.events.lock().unwrap().iter().filter(|e| f(e)).count();
    assert_eq!(count(&|e| matches!(e, SellingEvent::QuotationSent(e) if e.quotation_id == q)), 1);
    assert_eq!(count(&|e| matches!(e, SellingEvent::QuotationRejected(e) if e.quotation_id == q)), 1);
    assert_eq!(count(&|e| matches!(e, SellingEvent::QuotationReDrafted(e) if e.quotation_id == q)), 1);
    assert_eq!(count(&|e| matches!(e, SellingEvent::QuotationCancelled(e) if e.quotation_id == q)), 1);
    // a failed verb emits nothing.
    let before = rec.events.lock().unwrap().len();
    let _ = w.send_quotation(q, company).await.unwrap_err(); // cancelled, not draft
    assert_eq!(rec.events.lock().unwrap().len(), before);
}
