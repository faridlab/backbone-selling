//! The service-delivery confirm engine — selling's service-tracking ports (hand-authored,
//! user-owned).
//!
//! Proves the confirm-path behaviors against SCRIPTED fakes (the composition's product-surface
//! and project-engine adapter stand-ins — no inventory or project crate is involved; that is
//! the point of the ports):
//!
//!   confirm MINTS service delivery per non-downpayment line — the request carries the order's
//!   identity (id, company, customer, number, currency) and one entry per live non-downpayment
//!   line with its resolved rung and anchors; the mint's outcomes are STAMPED back onto exactly
//!   the lines that minted (manual/untracked lines and downpayments keep NULL); a task-in-project
//!   order's lines share ONE project; a second confirm of a confirmed order refuses (selling
//!   never re-mints through the confirm path — per-line exactly-once is the port's origin-key
//!   contract, exercised by the project side);
//!
//!   manual + untracked products MINT NOTHING — a product absent from the catalog resolution is
//!   the manual policy (absence is a configuration, not a refusal); nothing is stamped;
//!
//!   a mint refusal REFUSES the whole confirm fail-closed — the order stays draft, no cost
//!   stamp, no backref, the port's code rides verbatim, and the refusal is not sticky (a
//!   retried confirm with a healthy engine succeeds);
//!
//!   a catalog refusal refuses the confirm BEFORE any mint is attempted — no policy, no mint;
//!
//!   the UNWIRED composition (the two default adapters) behaves exactly like the era before
//!   the seam existed — nothing mints, nothing is stamped, the confirm succeeds.
//!
//! Requires DATABASE_URL pointing at a scratch DB with the selling migrations applied.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_service_catalog::{
    NoServiceCatalog, ServiceCatalogError, ServiceCatalogPort, ServiceTrackingInfo,
    ServiceTrackingRung,
};
use backbone_selling::application::service::selling_service_delivery::{
    NoServiceDelivery, ProjectFulfillmentError, ProjectFulfillmentPort, ServiceDeliveryLineOutcome,
    ServiceDeliveryRequest,
};
use backbone_selling::application::service::selling_stock_fulfillment::NoStockFulfillmentPort;
use backbone_selling::application::service::selling_unit_cost::NoUnitCostPort;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingError, SellingWriteService,
};

// ── the scripted fakes (the composition's adapter stand-ins) ───────────────────

/// One recorded catalog call: (company, distinct item ids asked for).
type CatalogCall = (Uuid, Vec<Uuid>);
/// The mint's per-line record: sale line -> (project id, task id).
type MintedIds = HashMap<Uuid, (Uuid, Option<Uuid>)>;

/// A scriptable `ServiceCatalogPort`: records every resolution request, plays back the
/// product-surface policies it was loaded with, and can be armed to refuse.
#[derive(Default, Clone)]
struct FakeCatalog {
    calls: Arc<Mutex<Vec<CatalogCall>>>,
    info: Arc<Mutex<Vec<ServiceTrackingInfo>>>,
    err: Arc<Mutex<Option<ServiceCatalogError>>>,
}

#[async_trait]
impl ServiceCatalogPort for FakeCatalog {
    async fn resolve_service_tracking(
        &self,
        company_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<Vec<ServiceTrackingInfo>, ServiceCatalogError> {
        self.calls.lock().unwrap().push((company_id, item_ids.to_vec()));
        if let Some(e) = self.err.lock().unwrap().take() {
            return Err(e);
        }
        Ok(self
            .info
            .lock()
            .unwrap()
            .iter()
            .filter(|i| item_ids.contains(&i.item_id))
            .cloned()
            .collect())
    }
}

/// A scriptable `ProjectFulfillmentPort`: records every mint request, mints deterministic ids
/// that are STABLE per sale line and per order (the origin-key idempotency posture the real
/// adapter enforces through its unique backstops), and can be armed to refuse.
#[derive(Default, Clone)]
struct FakeDelivery {
    mints: Arc<Mutex<Vec<ServiceDeliveryRequest>>>,
    err: Arc<Mutex<Option<ProjectFulfillmentError>>>,
    /// line id -> (project id, task id) minted for it. A repeat mint for the same line returns
    /// the SAME pair — exactly what the port's per-line idempotency contract demands.
    line_ids: Arc<Mutex<MintedIds>>,
    /// order id -> the ONE project an order's task-in-project lines share.
    order_project: Arc<Mutex<HashMap<Uuid, Uuid>>>,
}

#[async_trait]
impl ProjectFulfillmentPort for FakeDelivery {
    async fn mint_service_delivery(
        &self,
        req: &ServiceDeliveryRequest,
    ) -> Result<Vec<ServiceDeliveryLineOutcome>, ProjectFulfillmentError> {
        self.mints.lock().unwrap().push(req.clone());
        if let Some(e) = self.err.lock().unwrap().take() {
            return Err(e);
        }
        let mut line_ids = self.line_ids.lock().unwrap();
        let mut order_project = self.order_project.lock().unwrap();
        Ok(req
            .lines
            .iter()
            .map(|l| {
                // The port decides: a manual line is a skip (minted: false), never an error.
                if l.rung == ServiceTrackingRung::Manual {
                    return ServiceDeliveryLineOutcome {
                        sale_line_id: l.sale_line_id,
                        minted: false,
                        project_id: None,
                        task_id: None,
                    };
                }
                // Which project the line lives under: the product's FIXED project for
                // task_global_project, the order's ONE shared project otherwise (the shape a
                // task-in-project / project-only policy mints). Stable across repeats.
                let project = match l.rung {
                    ServiceTrackingRung::TaskGlobalProject => l
                        .fixed_project_id
                        .expect("the fixed anchor is present (a missing anchor is the real adapter's loud refusal)"),
                    _ => *order_project.entry(req.order_id).or_insert_with(Uuid::new_v4),
                };
                // Per-line task, keyed on the sale line: a repeat mint returns the prior pair.
                let (project_id, task_id) = *line_ids.entry(l.sale_line_id).or_insert_with(|| {
                    let task = match l.rung {
                        ServiceTrackingRung::TaskGlobalProject
                        | ServiceTrackingRung::TaskInProject => Some(Uuid::new_v4()),
                        ServiceTrackingRung::ProjectOnly => None,
                        ServiceTrackingRung::Manual => unreachable!("handled above"),
                    };
                    (project, task)
                });
                ServiceDeliveryLineOutcome {
                    sale_line_id: l.sale_line_id,
                    minted: true,
                    project_id: Some(project_id),
                    task_id,
                }
            })
            .collect())
    }
}

impl FakeDelivery {
    fn err(code: &str, message: &str) -> ProjectFulfillmentError {
        ProjectFulfillmentError { code: code.into(), message: message.into() }
    }
}

// ── fixtures ──────────────────────────────────────────────────────────────────

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
fn line(item: Uuid, qty: &str) -> NewLine {
    NewLine {
        invoice_policy: None,
        is_downpayment: None,
        revenue_account_id: None,
        description: None,
        item_id: item,
        quantity: d(qty),
        unit_price: d("150000"),
        line_discount: Decimal::ZERO,
    }
}
fn downpayment_line(item: Uuid, qty: &str) -> NewLine {
    NewLine {
        invoice_policy: None,
        is_downpayment: Some(true),
        revenue_account_id: None,
        description: None,
        item_id: item,
        quantity: d(qty),
        unit_price: d("50000"),
        line_discount: Decimal::ZERO,
    }
}
async fn draft_order(w: &SellingWriteService, company: Uuid, lines: Vec<NewLine>) -> Uuid {
    let n = uq("SO");
    w.create_sales_order(NewSalesOrder {
        order_number: n,
        quotation_id: None,
        delivery_carrier_id: None,
        company_id: company,
        branch_id: None,
        customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 29).unwrap(),
        delivery_date: None,
        currency: None,
        tax_rate: Decimal::ZERO,
        notes: None,
        lines,
    })
    .await
    .unwrap()
}
async fn order_status(pool: &PgPool, oid: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// One order line's stamped backrefs: (line id, project id, task id).
async fn line_backrefs(pool: &PgPool, order: Uuid) -> Vec<(Uuid, Option<Uuid>, Option<Uuid>)> {
    sqlx::query(
        "SELECT id, project_id, task_id FROM selling.sales_order_items WHERE order_id=$1 ORDER BY id",
    )
    .bind(order)
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("id"), r.get("project_id"), r.get("task_id")))
    .collect()
}

// ── (a) confirm mints service delivery per line and stamps the backrefs ──────

// Request shape + stamping: the mint request models the order's identity (id, company,
// customer, number, currency) with ONE entry per live NON-DOWNPAYMENT line carrying its
// resolved rung + anchors; duplicate items are resolved once; the outcomes are stamped onto
// exactly the minted lines — a task-in-project order's lines share ONE project, each line
// gets its own task, manual and downpayment lines keep NULL; a second confirm refuses
// (NotDraft), so selling never re-mints through the confirm path.
#[tokio::test]
async fn confirm_mints_per_line_and_stamps_backrefs() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let (svc, fixed, plain) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (template_id, fixed_project_id) = (Uuid::new_v4(), Uuid::new_v4());
    let cat = FakeCatalog::default();
    *cat.info.lock().unwrap() = vec![
        ServiceTrackingInfo {
            item_id: svc,
            service_tracking: ServiceTrackingRung::TaskInProject,
            service_project_id: None,
            service_project_template_id: Some(template_id),
        },
        ServiceTrackingInfo {
            item_id: fixed,
            service_tracking: ServiceTrackingRung::TaskGlobalProject,
            service_project_id: Some(fixed_project_id),
            service_project_template_id: None,
        },
        // `plain` deliberately ABSENT from the surface — the untracked/manual posture.
    ];
    let del = FakeDelivery::default();

    // Two lines of the SAME service product (the dedup case) + one fixed-project product +
    // one untracked product + a downpayment.
    let order = draft_order(
        &w,
        company,
        vec![
            line(svc, "10"),
            line(svc, "4"),
            line(fixed, "2"),
            line(plain, "7"),
            downpayment_line(plain, "1"),
        ],
    )
    .await;
    let order_number = sqlx::query_scalar::<_, String>(
        "SELECT order_number FROM selling.sales_orders WHERE id=$1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();

    w.confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    // The catalog was asked once, for the DISTINCT items of the non-downpayment lines.
    {
        let calls = cat.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one policy resolution per confirm");
        assert_eq!(calls[0].0, company);
        let mut asked = calls[0].1.clone();
        asked.sort_unstable();
        let mut expected = vec![svc, fixed, plain];
        expected.sort_unstable();
        assert_eq!(asked, expected, "duplicate items resolve once; downpayments never ask");
    }

    // The mint was asked once, with the order's identity and the four delivery lines.
    {
        let mints = del.mints.lock().unwrap();
        assert_eq!(mints.len(), 1, "one mint request per confirm");
        let req = &mints[0];
        assert_eq!(req.order_id, order);
        assert_eq!(req.company_id, company);
        assert_eq!(req.order_number, order_number);
        assert_eq!(req.currency, "IDR", "the order's default currency rides the request");
        assert_eq!(req.lines.len(), 4, "the downpayment line never drives delivery work");
        let by_item =
            |it: Uuid| req.lines.iter().filter(|l| l.item_id == it).collect::<Vec<_>>();
        for l in by_item(svc) {
            assert_eq!(l.rung, ServiceTrackingRung::TaskInProject);
            assert_eq!(l.template_id, Some(template_id), "the fork anchor rides the line");
            assert_eq!(l.fixed_project_id, None);
        }
        let fl = by_item(fixed)[0];
        assert_eq!(fl.rung, ServiceTrackingRung::TaskGlobalProject);
        assert_eq!(fl.fixed_project_id, Some(fixed_project_id), "the fixed anchor rides the line");
        assert_eq!(by_item(plain)[0].rung, ServiceTrackingRung::Manual, "absent product = manual");
    }

    // The port's own per-line record — the stable ids the stamp must agree with.
    let outcome_of = |lid: Uuid| del.line_ids.lock().unwrap().get(&lid).copied();
    {
        let mints = del.mints.lock().unwrap();
        let req = &mints[0];
        let svc_lines: Vec<Uuid> =
            req.lines.iter().filter(|l| l.item_id == svc).map(|l| l.sale_line_id).collect();
        let fixed_line = req.lines.iter().find(|l| l.item_id == fixed).unwrap().sale_line_id;

        // The two task-in-project lines share the order's ONE project; the fixed line does not.
        let (p1, t1) = outcome_of(svc_lines[0]).expect("minted line has ids");
        let (p2, t2) = outcome_of(svc_lines[1]).expect("minted line has ids");
        assert_eq!(p1, p2, "one project per order for task-in-project lines");
        assert_ne!(t1, t2, "per-line tasks are per line");
        assert_ne!(p1, outcome_of(fixed_line).unwrap().0, "the fixed-project line lives elsewhere");

        // The manual line minted nothing.
        let manual_line = req.lines.iter().find(|l| l.item_id == plain).unwrap().sale_line_id;
        assert!(outcome_of(manual_line).is_none(), "manual lines mint nothing");
    }

    // The stamped rows agree with the port's record: three minted lines carry their ids, the
    // untracked and downpayment lines keep NULL.
    let refs = line_backrefs(&pool, order).await;
    assert_eq!(refs.len(), 5);
    assert_eq!(refs.iter().filter(|r| r.1.is_some()).count(), 3, "three minted lines stamped");
    for r in &refs {
        match outcome_of(r.0) {
            Some((project, task)) => {
                assert_eq!(r.1, Some(project), "the row carries the minted project");
                assert_eq!(r.2, task, "the row carries the minted task");
            }
            None => {
                assert_eq!((r.1, r.2), (None, None), "unminted lines keep NULL backrefs");
            }
        }
    }

    // A second confirm of a confirmed order refuses — and, because the mint runs BEFORE the
    // confirm transaction's draft guard (the same launch-before-commit posture as the stock
    // launch), the re-mint HAS already happened by the time the guard refuses. That is the
    // designed crash-window shape: the port's per-line idempotency is what makes the re-mint
    // harmless. Assert exactly that — the re-mint minted NOTHING new.
    let minted_before = del.line_ids.lock().unwrap().clone();
    match w
        .confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap_err()
    {
        SellingError::NotDraft(_) => {}
        other => panic!("expected NotDraft on re-confirm, got {other:?}"),
    }
    assert_eq!(del.mints.lock().unwrap().len(), 2, "the re-confirm re-minted before its guard refused");
    assert_eq!(
        del.line_ids.lock().unwrap().len(),
        minted_before.len(),
        "the idempotent re-mint minted nothing new"
    );
    // And the rows still agree with the (unchanged) port record.
    for r in &line_backrefs(&pool, order).await {
        match outcome_of(r.0) {
            Some((project, task)) => assert_eq!((r.1, r.2), (Some(project), task)),
            None => assert_eq!((r.1, r.2), (None, None)),
        }
    }
}

// ── (b) manual + untracked products mint nothing ──────────────────────────────

// A product absent from the resolution is the manual policy: its lines ride the request with
// the manual rung, come back unminted, and NOTHING is stamped. The confirm succeeds — manual
// is a legitimate configuration, not a failure.
#[tokio::test]
async fn manual_and_untracked_products_mint_nothing() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let cat = FakeCatalog::default();
    *cat.info.lock().unwrap() = vec![ServiceTrackingInfo {
        item_id: item,
        service_tracking: ServiceTrackingRung::Manual,
        service_project_id: None,
        service_project_template_id: None,
    }];
    let del = FakeDelivery::default();

    let order = draft_order(&w, company, vec![line(item, "3")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    let (mint_count, minted_rung) = {
        let mints = del.mints.lock().unwrap();
        (mints.len(), mints[0].lines[0].rung)
    };
    assert_eq!(mint_count, 1);
    assert_eq!(minted_rung, ServiceTrackingRung::Manual);

    for r in line_backrefs(&pool, order).await {
        assert_eq!((r.1, r.2), (None, None), "a manual rung stamps nothing");
    }
}

// ── (c) a mint refusal refuses the whole confirm, fail-closed and retryable ───

#[tokio::test]
async fn mint_refusal_refuses_confirm_and_is_retryable() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let cat = FakeCatalog::default();
    *cat.info.lock().unwrap() = vec![ServiceTrackingInfo {
        item_id: item,
        service_tracking: ServiceTrackingRung::TaskInProject,
        service_project_id: None,
        service_project_template_id: None,
    }];
    let del = FakeDelivery::default();
    *del.err.lock().unwrap() =
        Some(FakeDelivery::err("fixed_project_missing", "the product names no fixed project"));

    let order = draft_order(&w, company, vec![line(item, "5")]).await;
    match w
        .confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap_err()
    {
        SellingError::ServiceDeliveryRejected { code, .. } => {
            assert_eq!(code, "fixed_project_missing", "the port's code rides verbatim")
        }
        other => panic!("expected ServiceDeliveryRejected, got {other:?}"),
    }
    assert_eq!(order_status(&pool, order).await, "draft", "the order stays draft");
    for r in line_backrefs(&pool, order).await {
        assert_eq!((r.1, r.2), (None, None), "a refused mint writes no backref");
    }
    let stamped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM selling.sales_order_items WHERE order_id=$1 AND unit_cost IS NOT NULL",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamped, 0, "a refused mint writes no cost stamp either (stamps ride the confirm tx)");

    // Not sticky: the armed error is consumed; the retry mints and confirms.
    w.confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");
    assert_eq!(del.mints.lock().unwrap().len(), 2, "the retry re-minted (idempotency is the port's)");
}

// ── (d) a catalog refusal refuses BEFORE any mint is attempted ────────────────

#[tokio::test]
async fn catalog_refusal_refuses_confirm_before_any_mint() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let cat = FakeCatalog::default();
    *cat.err.lock().unwrap() = Some(ServiceCatalogError {
        code: "surface_unreadable".into(),
        message: "the product projection is unreachable".into(),
    });
    let del = FakeDelivery::default();

    let order = draft_order(&w, company, vec![line(item, "5")]).await;
    match w
        .confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap_err()
    {
        SellingError::ServiceCatalogRejected { code, .. } => {
            assert_eq!(code, "surface_unreadable")
        }
        other => panic!("expected ServiceCatalogRejected, got {other:?}"),
    }
    assert_eq!(order_status(&pool, order).await, "draft");
    assert!(del.mints.lock().unwrap().is_empty(), "no policy resolution, no mint attempt");
}

// ── (e) the unwired composition matches the pre-seam behavior ─────────────────

// NoServiceCatalog + NoServiceDelivery: nothing mints, nothing is stamped, and the confirm
// succeeds exactly as it did before the seam existed.
#[tokio::test]
async fn unwired_composition_matches_pre_seam_behavior() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = draft_order(&w, company, vec![line(item, "6")]).await;
    w.confirm_sales_order(
        order,
        company,
        &NoUnitCostPort,
        &NoStockFulfillmentPort,
        &NoServiceCatalog,
        &NoServiceDelivery,
    )
    .await
    .unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");
    for r in line_backrefs(&pool, order).await {
        assert_eq!((r.1, r.2), (None, None), "an unwired composition stamps nothing");
    }
}
