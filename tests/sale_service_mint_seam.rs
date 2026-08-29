//! The REAL-MINT service-delivery seam — selling's confirm engine driving the REAL
//! backbone-project mint (hand-authored, user-owned).
//!
//! `sale_service_confirm.rs` proves the confirm engine against SCRIPTED fakes; backbone-project's
//! own suite proves the mint against real tables. Neither drives one through the other — this file
//! does, through an in-test adapter that maps this module's `ProjectFulfillmentPort` DTOs onto
//! project's `ProjectWriteService::mint_service_delivery` exactly the way a composing service
//! would (the DTOs are duplicated per side by design; the mapping is the seam):
//!
//!   SSMS-1 (confirm mints REAL project work): a confirmed order's service lines mint actual
//!            `project.projects` / `project.tasks` rows — one per-order project (keyed
//!            `source_so_id`, forked from the product's template with its blueprint tasks
//!            materialized), one task per line keyed by the sale line (origin-key unique), the
//!            fixed-project line's task under its fixed project — and the outcomes are stamped
//!            back onto exactly the minted order lines.
//!
//!   SSMS-2 (the idempotent repeat): re-minting the SAME request through the adapter returns the
//!            SAME stable ids with `minted: false` everywhere (the origin-key uniques'
//!            re-select arm), minting nothing new; the re-CONFIRM path (the crash-window shape —
//!            selling launches the mint before its draft guard) re-invokes the port and still
//!            mints nothing new.
//!
//!   SSMS-3 (fail-closed refusal): a fixed-project line whose anchor lives in ANOTHER company is
//!            invisible to the company-scoped mint — the port refuses, the whole confirm refuses,
//!            the order stays draft, and no project work exists.
//!
//! Requires DATABASE_URL pointing at a scratch DB with BOTH the selling and the project
//! migrations applied (the module checkout resolved by the dev-dependency is the tagged tree).
//!
//! Test-only edge, dev-dependency ONLY — the shipped selling library has zero normal Cargo edge
//! to backbone-project (verify: `cargo tree -e normal -i backbone-project` is empty).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_project::application::service::project_write_service::{
    NewProject, ProjectError, ProjectWriteService,
};
use backbone_selling::application::service::selling_service_catalog::{
    ServiceCatalogError, ServiceCatalogPort, ServiceTrackingInfo, ServiceTrackingRung,
};
use backbone_selling::application::service::selling_service_delivery::{
    ProjectFulfillmentError, ProjectFulfillmentPort, ServiceDeliveryLineOutcome,
    ServiceDeliveryRequest,
};
use backbone_selling::application::service::selling_stock_fulfillment::NoStockFulfillmentPort;
use backbone_selling::application::service::selling_unit_cost::NoUnitCostPort;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingError, SellingWriteService,
};

// ── the real-mint adapter (the composition's stand-in for the host's future adapter) ──

/// Selling's `ProjectFulfillmentPort` implemented over the REAL backbone-project write service:
/// maps the port DTOs onto project's mint vocabulary, records every request it carried (so tests
/// can replay the idempotency contract through the exact same adapter), and flattens project's
/// error taxonomy into the port's `{code, message}` refusal.
struct RealProjectFulfillment {
    svc: ProjectWriteService,
    requests: Mutex<Vec<ServiceDeliveryRequest>>,
}

impl RealProjectFulfillment {
    fn new(pool: PgPool) -> Self {
        Self { svc: ProjectWriteService::new(pool), requests: Mutex::new(Vec::new()) }
    }

    /// The requests carried so far (cloned), oldest first.
    fn carried(&self) -> Vec<ServiceDeliveryRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Map the port's error taxonomy onto the flat refusal — the implementing side keeps its own
    /// vocabulary on its side of the seam; only a stable `code` rides across.
    fn reject(e: ProjectError) -> ProjectFulfillmentError {
        let code = match &e {
            ProjectError::Db(_) => "project_db",
            ProjectError::NotFound(_) => "project_not_found",
            ProjectError::InvalidState(_) => "project_invalid_state",
            ProjectError::Invalid(_) => "project_invalid_input",
            ProjectError::BillingRejected(_) => "project_billing_rejected",
            ProjectError::Guarded(_) => "project_guarded",
        };
        ProjectFulfillmentError { code: code.into(), message: e.to_string() }
    }
}

#[async_trait]
impl ProjectFulfillmentPort for RealProjectFulfillment {
    async fn mint_service_delivery(
        &self,
        req: &ServiceDeliveryRequest,
    ) -> Result<Vec<ServiceDeliveryLineOutcome>, ProjectFulfillmentError> {
        self.requests.lock().unwrap().push(req.clone());
        // The DTO mapping IS the seam: same wire vocabulary, duplicated per side by design.
        let mapped = backbone_project::application::service::project_write_service::ServiceDeliveryRequest {
            order_id: req.order_id,
            company_id: req.company_id,
            customer_id: req.customer_id,
            order_number: req.order_number.clone(),
            currency: req.currency.clone(),
            lines: req
                .lines
                .iter()
                .map(|l| {
                    let rung = match l.rung {
                        ServiceTrackingRung::TaskGlobalProject => {
                            backbone_project::application::service::project_write_service::ServiceTrackingRung::TaskGlobalProject
                        }
                        ServiceTrackingRung::TaskInProject => {
                            backbone_project::application::service::project_write_service::ServiceTrackingRung::TaskInProject
                        }
                        ServiceTrackingRung::ProjectOnly => {
                            backbone_project::application::service::project_write_service::ServiceTrackingRung::ProjectOnly
                        }
                        ServiceTrackingRung::Manual => {
                            backbone_project::application::service::project_write_service::ServiceTrackingRung::Manual
                        }
                    };
                    backbone_project::application::service::project_write_service::ServiceDeliveryLine {
                        sale_line_id: l.sale_line_id,
                        item_id: l.item_id,
                        quantity: l.quantity,
                        description: l.description.clone(),
                        rung,
                        fixed_project_id: l.fixed_project_id,
                        template_id: l.template_id,
                    }
                })
                .collect(),
        };
        let outcomes = self
            .svc
            .mint_service_delivery(&mapped)
            .await
            .map_err(Self::reject)?;
        Ok(outcomes
            .into_iter()
            .map(|o| ServiceDeliveryLineOutcome {
                sale_line_id: o.sale_line_id,
                minted: o.minted,
                project_id: o.project_id,
                task_id: o.task_id,
            })
            .collect())
    }
}

// ── the scripted catalog (selling-side; identical posture to sale_service_confirm.rs) ──

/// A scriptable `ServiceCatalogPort`: plays back the product-surface policies it was loaded with.
/// The catalog half is NOT under test here — it only feeds the real policies the mint must honor.
#[derive(Default, Clone)]
struct FakeCatalog {
    info: Arc<Mutex<Vec<ServiceTrackingInfo>>>,
}

#[async_trait]
impl ServiceCatalogPort for FakeCatalog {
    async fn resolve_service_tracking(
        &self,
        _company_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<Vec<ServiceTrackingInfo>, ServiceCatalogError> {
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

/// One order line's stamped backrefs: (line id, project id, task id) in insertion order.
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

/// Seed one active project template (project_type `external`) with two blueprint tasks.
async fn seed_template(pool: &PgPool, company: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO project.project_templates (id, company_id, template_name, project_type, status)
           VALUES ($1,$2,'Installation blueprint','external','active')"#,
    )
    .bind(id)
    .bind(company)
    .execute(pool)
    .await
    .unwrap();
    for (subject, seq) in [("Site survey", 1), ("Commissioning", 2)] {
        sqlx::query(
            r#"INSERT INTO project.project_template_tasks (template_id, company_id, subject, expected_time, sequence)
               VALUES ($1,$2,$3,0,$4)"#,
        )
        .bind(id)
        .bind(company)
        .bind(subject)
        .bind(seq)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

/// The live project minted for a sales order (NULL when the order minted none), plus the live
/// task count under a project (blueprint tasks have NULL origin lines; line tasks carry theirs).
async fn order_project(pool: &PgPool, company: Uuid, order: Uuid) -> Option<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM project.projects WHERE company_id=$1 AND source_so_id=$2",
    )
    .bind(company)
    .bind(order)
    .fetch_optional(pool)
    .await
    .unwrap()
}
async fn tasks_of(pool: &PgPool, project: Uuid) -> Vec<(Uuid, Option<Uuid>, String)> {
    sqlx::query(
        "SELECT id, origin_sale_line_id, subject FROM project.tasks WHERE project_id=$1 ORDER BY subject, id",
    )
    .bind(project)
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get::<Uuid,_>("id"), r.get::<Option<Uuid>,_>("origin_sale_line_id"), r.get::<String,_>("subject")))
    .collect()
}

// ── SSMS-1 + SSMS-2: one confirm-mint round-trip and its idempotent repeat ─────

// Confirm mints REAL project work through the seam; a repeated mint of the same request returns
// the same stable ids minting nothing; a re-CONFIRM (the crash-window shape: the mint launches
// before selling's draft guard) re-invokes the port and still mints nothing new.
#[tokio::test]
async fn confirm_mints_real_project_work_and_the_repeat_mints_nothing() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let projects = ProjectWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let (tpl_item, fixed_item, plain_item) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    // The product surface: a template-forking per-order service, a fixed-project service, and an
    // untracked product (absent from the resolution — the manual policy).
    let template_id = seed_template(&pool, company).await;
    let fixed_project = projects
        .create_project(NewProject {
            company_id: company,
            project_name: "Global service desk".into(),
            project_type: "internal".into(),
            customer_id: None,
            source_so_id: None,
            currency: Some("IDR".into()),
        })
        .await
        .unwrap();
    let cat = FakeCatalog::default();
    *cat.info.lock().unwrap() = vec![
        ServiceTrackingInfo {
            item_id: tpl_item,
            service_tracking: ServiceTrackingRung::TaskInProject,
            service_project_id: None,
            service_project_template_id: Some(template_id),
        },
        ServiceTrackingInfo {
            item_id: fixed_item,
            service_tracking: ServiceTrackingRung::TaskGlobalProject,
            service_project_id: Some(fixed_project),
            service_project_template_id: None,
        },
        // plain_item deliberately ABSENT — the untracked/manual posture.
    ];
    let del = RealProjectFulfillment::new(pool.clone());

    // Two lines of the template-forking service (one project per ORDER, one task per line) + one
    // fixed-project line + one untracked line + a downpayment.
    let order = draft_order(
        &w,
        company,
        vec![
            line(tpl_item, "10"),
            line(tpl_item, "4"),
            line(fixed_item, "2"),
            line(plain_item, "7"),
            downpayment_line(plain_item, "1"),
        ],
    )
    .await;
    let (order_number, order_customer): (String, Uuid) = sqlx::query_as(
        "SELECT order_number, customer_id FROM selling.sales_orders WHERE id=$1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();

    // SSMS-1 — the confirm drives the REAL mint and stamps its outcomes.
    w.confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    let carried = del.carried();
    assert_eq!(carried.len(), 1, "one mint request per confirm");
    let req = carried[0].clone();
    assert_eq!(req.order_id, order);
    assert_eq!(req.company_id, company);
    assert_eq!(req.lines.len(), 4, "the downpayment line never drives delivery work");

    // The per-order project exists ONCE, keyed by the sales order, named after it, for its
    // customer, in the order's currency, and forked with the template's project type.
    let minted_project = order_project(&pool, company, order)
        .await
        .expect("the order minted its project");
    {
        let (name, customer, currency, ptype, status): (String, Uuid, String, String, String) =
            sqlx::query_as(
                r#"SELECT project_name, customer_id, currency, project_type::text, status::text
                   FROM project.projects WHERE id=$1"#,
            )
            .bind(minted_project)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, format!("SO {order_number}"));
        assert_eq!(customer, order_customer, "the minted project carries the order's customer");
        assert_eq!(currency, "IDR");
        assert_eq!(ptype, "external", "the fork took the template's project type");
        assert_eq!(status, "open");
    }

    // The order project carries the TWO blueprint tasks (origin NULL) plus one task per
    // template-forking line (origin = the sale line) — 2 + 2.
    let order_tasks = tasks_of(&pool, minted_project).await;
    assert_eq!(order_tasks.len(), 4, "2 blueprint tasks + 2 per-line tasks");
    let blueprint: Vec<&(Uuid, Option<Uuid>, String)> =
        order_tasks.iter().filter(|t| t.1.is_none()).collect();
    assert_eq!(blueprint.len(), 2, "the template's tasks materialized into the fork");
    assert!(blueprint.iter().any(|t| t.2 == "Site survey"));
    assert!(blueprint.iter().any(|t| t.2 == "Commissioning"));

    // The fixed-project line's task lives under the FIXED project, keyed by its sale line.
    let fixed_line = req.lines.iter().find(|l| l.item_id == fixed_item).unwrap().sale_line_id;
    let fixed_tasks = tasks_of(&pool, fixed_project).await;
    assert_eq!(fixed_tasks.len(), 1, "one task for the fixed-project line");
    assert_eq!(fixed_tasks[0].1, Some(fixed_line), "the task is keyed by its sale line");

    // The stamped backrefs agree with the real rows: three minted lines carry ids, the untracked
    // and downpayment lines keep NULL.
    let refs = line_backrefs(&pool, order).await;
    assert_eq!(refs.len(), 5);
    assert_eq!(refs.iter().filter(|r| r.1.is_some()).count(), 3, "three minted lines stamped");
    let by_line = |lid: Uuid| refs.iter().find(|r| r.0 == lid).copied();
    for l in req.lines.iter().filter(|l| l.rung != ServiceTrackingRung::Manual) {
        let (_, project, task) = by_line(l.sale_line_id).unwrap();
        let project = project.expect("minted line carries its project");
        let task = task.expect("a task-bearing rung carries its task");
        let expected_project =
            if l.rung == ServiceTrackingRung::TaskGlobalProject { fixed_project } else { minted_project };
        assert_eq!(project, expected_project);
        let in_db = tasks_of(&pool, project).await;
        assert!(
            in_db.iter().any(|t| t.0 == task && t.1 == Some(l.sale_line_id)),
            "the stamped task is the origin-keyed row in project.tasks"
        );
    }
    let manual_line = req.lines.iter().find(|l| l.rung == ServiceTrackingRung::Manual).unwrap().sale_line_id;
    assert_eq!(by_line(manual_line).unwrap().1, None, "the untracked line stamps nothing");

    // SSMS-2 — the idempotent repeat: the SAME request through the SAME adapter returns the SAME
    // stable ids with minted:false everywhere, and mints nothing new.
    let total_tasks_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project.tasks WHERE company_id=$1",
    )
    .bind(company)
    .fetch_one(&pool)
    .await
    .unwrap();
    let repeat = ProjectFulfillmentPort::mint_service_delivery(&del, &req).await.unwrap();
    assert_eq!(repeat.len(), req.lines.len(), "one outcome per line, in input order");
    for (i, o) in repeat.iter().enumerate() {
        assert_eq!(o.sale_line_id, req.lines[i].sale_line_id, "outcomes keep input order");
        assert!(!o.minted, "the repeat minted nothing (line {})", i);
    }
    let repeat_ids: HashMap<Uuid, (Option<Uuid>, Option<Uuid>)> =
        repeat.iter().map(|o| (o.sale_line_id, (o.project_id, o.task_id))).collect();
    for l in &req.lines {
        let stamped = by_line(l.sale_line_id).unwrap();
        match stamped.1 {
            Some(p) => {
                let (rp, rt) = repeat_ids[&l.sale_line_id];
                assert_eq!(rp, Some(p), "the repeat reports the STABLE project id");
                assert_eq!(rt, stamped.2, "the repeat reports the STABLE task id");
            }
            None => {
                assert_eq!(repeat_ids[&l.sale_line_id], (None, None), "a manual line stays unminted");
            }
        }
    }
    assert_eq!(
        order_project(&pool, company, order).await,
        Some(minted_project),
        "still exactly the one order project"
    );
    let total_tasks_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project.tasks WHERE company_id=$1",
    )
    .bind(company)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total_tasks_before, total_tasks_after, "the repeat minted zero rows");

    // And the re-CONFIRM (the crash-window shape): selling re-launches the mint BEFORE its draft
    // guard refuses — the port contract must make that re-mint harmless.
    match w
        .confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap_err()
    {
        SellingError::NotDraft(_) => {}
        other => panic!("expected NotDraft on re-confirm, got {other:?}"),
    }
    // Three carries total by now: the confirm's mint, the manual replay above, and the
    // re-confirm's re-mint — which launched before the guard refused and minted nothing.
    assert_eq!(del.carried().len(), 3, "the re-confirm re-minted before its guard refused");
    let total_tasks_reconfirm: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project.tasks WHERE company_id=$1",
    )
    .bind(company)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total_tasks_before, total_tasks_reconfirm, "the re-confirm minted zero rows too");
    for r in &line_backrefs(&pool, order).await {
        if let Some(ids) = repeat_ids.get(&r.0) {
            assert_eq!((r.1, r.2), *ids, "the rows still agree with the stable ids");
        } else {
            assert_eq!((r.1, r.2), (None, None), "the downpayment line stays unstamped");
        }
    }
}

// ── SSMS-3: an invisible fixed anchor refuses the confirm, fail-closed ─────────

// The mint's fixed-project lookup only sees LIVE projects in scope: an anchor that was
// soft-deleted is simply not found, the port refuses, and the whole confirm refuses BEFORE any
// work exists — the order stays draft and no project/task row appears for it.
//
// (The CROSS-company anchor case is the same not-found refusal, but its invisibility is enforced
// by the row-level-security fence — `app.company_id` policy — not by a SQL filter, so it is only
// observable under a fenced role; module test pools run as the migration superuser and bypass
// RLS. The composed host runs fenced; that is where that arm holds.)
#[tokio::test]
async fn soft_deleted_fixed_anchor_refuses_confirm_fail_closed() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let projects = ProjectWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();

    let anchor = projects
        .create_project(NewProject {
            company_id: company,
            project_name: "Retired service desk".into(),
            project_type: "internal".into(),
            customer_id: None,
            source_so_id: None,
            currency: Some("IDR".into()),
        })
        .await
        .unwrap();
    sqlx::query(r#"UPDATE project.projects SET metadata = jsonb_set(metadata, '{deleted_at}', to_jsonb(NOW())) WHERE id=$1"#)
        .bind(anchor)
        .execute(&pool)
        .await
        .unwrap();
    let cat = FakeCatalog::default();
    *cat.info.lock().unwrap() = vec![ServiceTrackingInfo {
        item_id: item,
        service_tracking: ServiceTrackingRung::TaskGlobalProject,
        service_project_id: Some(anchor),
        service_project_template_id: None,
    }];
    let del = RealProjectFulfillment::new(pool.clone());

    let order = draft_order(&w, company, vec![line(item, "5")]).await;
    match w
        .confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &cat, &del)
        .await
        .unwrap_err()
    {
        SellingError::ServiceDeliveryRejected { code, .. } => {
            assert_eq!(code, "project_invalid_input", "the mapped refusal code rides verbatim")
        }
        other => panic!("expected ServiceDeliveryRejected, got {other:?}"),
    }
    assert_eq!(order_status(&pool, order).await, "draft", "the order stays draft");
    assert!(order_project(&pool, company, order).await.is_none(), "no project was minted");
    let seeded_tasks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project.tasks WHERE company_id=$1",
    )
    .bind(company)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(seeded_tasks, 0, "no task was minted");
    for r in line_backrefs(&pool, order).await {
        assert_eq!((r.1, r.2), (None, None), "a refused mint writes no backref");
    }
}
