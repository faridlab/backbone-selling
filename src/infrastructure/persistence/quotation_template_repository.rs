//! Repository for QuotationTemplate entities
//!
//! Hand-authored and declared `user_owned` in `metaphor.codegen.yaml` (the generator skips this
//! path wholesale). Holds the hand-written template SQL: the find/list the write service uses when
//! stamping a new quotation, the duplicate-name pre-read, and the guarded insert the guarded route
//! backs. (4-layer rule: services orchestrate and own the unit of work, repositories hold the SQL.)
//!
//! Thin newtype over `backbone_orm::GenericCrudRepository<QuotationTemplate, backbone_orm::SoftDelete>`.
//! All standard CRUD methods are available via `Deref`.

use anyhow::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

// The optional-row read and write twins ride the org-scope module; the all-rows read twin lives
// only in the legacy `company_scope` module. Their connection discipline is the request-dedicated
// connection when the composing service bound one, plain pool otherwise. The legacy task-local
// branch is never taken: this module sets no legacy scope of its own (ADR-0029).
use backbone_orm::company_scope::fetch_all_rows_scoped;
use backbone_orm::org_scope;

use crate::domain::entity::QuotationTemplate;

/// Table name for QuotationTemplate entities
pub const TABLE_NAME: &str = "selling.quotation_templates";

/// Repository for QuotationTemplate entities.
///
/// All standard CRUD, soft-delete, pagination, and bulk methods are
/// provided automatically via `Deref` to `backbone_orm::GenericCrudRepository`.
pub struct QuotationTemplateRepository(
    backbone_orm::GenericCrudRepository<QuotationTemplate, backbone_orm::SoftDelete>,
);

impl std::ops::Deref for QuotationTemplateRepository {
    type Target = backbone_orm::GenericCrudRepository<QuotationTemplate, backbone_orm::SoftDelete>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl QuotationTemplateRepository {
    /// Create a new repository instance.
    pub fn new(pool: PgPool) -> Self {
        Self(backbone_orm::GenericCrudRepository::new(pool, TABLE_NAME))
    }
}

/// One quotation-template row as the write service consumes it.
pub struct QuotationTemplateRow {
    pub id: Uuid,
    pub name: String,
    pub validity_days: i32,
    pub default_notes: Option<String>,
}

/// The exact row a template insert writes.
pub struct NewQuotationTemplateRow<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub validity_days: i32,
    pub default_notes: Option<&'a str>,
}

/// Hand-written QuotationTemplate SQL. Lives here (not in the write service) per the module's
/// 4-layer rule.
impl QuotationTemplateRepository {
    /// Read one template by id: under a composed tenancy decorator another unit's template id
    /// simply isn't found (no existence leak).
    pub async fn find_template(
        &self,
        pool: &PgPool,
        template_id: Uuid,
    ) -> Result<Option<QuotationTemplateRow>, sqlx::Error> {
        let row = org_scope::fetch_optional_row_scoped(
            pool,
            sqlx::query(
                r#"SELECT id, name, validity_days, default_notes
                   FROM selling.quotation_templates
                   WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
            )
            .bind(template_id),
        )
        .await?;
        Ok(row.map(|r| Self::project(&r)))
    }

    /// Read one live template by name — the duplicate-name pre-read behind
    /// `create_quotation_template`'s refusal. `Ok(None)` = the name is free.
    pub async fn find_template_by_name(
        &self,
        pool: &PgPool,
        name: &str,
    ) -> Result<Option<QuotationTemplateRow>, sqlx::Error> {
        let row = org_scope::fetch_optional_row_scoped(
            pool,
            sqlx::query(
                r#"SELECT id, name, validity_days, default_notes
                   FROM selling.quotation_templates
                   WHERE name=$1 AND (metadata->>'deleted_at') IS NULL"#,
            )
            .bind(name),
        )
        .await?;
        Ok(row.map(|r| Self::project(&r)))
    }

    /// List the templates (name order). Under a composed tenancy decorator the request's fence
    /// scopes the read; an undecorated deployment lists all.
    pub async fn list_templates(&self, pool: &PgPool) -> Result<Vec<QuotationTemplateRow>, sqlx::Error> {
        let rows = fetch_all_rows_scoped(
            pool,
            sqlx::query(
                r#"SELECT id, name, validity_days, default_notes
                   FROM selling.quotation_templates
                   WHERE (metadata->>'deleted_at') IS NULL
                   ORDER BY name"#,
            ),
        )
        .await?;
        Ok(rows.iter().map(|r| Self::project(r)).collect())
    }

    /// Insert one template. Returns the raw `sqlx::Error` deliberately: the caller inspects it
    /// for a unique violation on the name (the decorator's per-unit unique under a composed
    /// tenancy) to turn it into a domain error.
    pub async fn insert_template(
        &self,
        pool: &PgPool,
        t: &NewQuotationTemplateRow<'_>,
    ) -> Result<(), sqlx::Error> {
        org_scope::execute_scoped(
            pool,
            sqlx::query(
                r#"INSERT INTO selling.quotation_templates (id, name, validity_days, default_notes)
                   VALUES ($1,$2,$3,$4)"#,
            )
            .bind(t.id)
            .bind(t.name)
            .bind(t.validity_days)
            .bind(t.default_notes),
        )
        .await?;
        Ok(())
    }

    /// The row projection shared by the reads above.
    fn project(r: &sqlx::postgres::PgRow) -> QuotationTemplateRow {
        QuotationTemplateRow {
            id: r.get("id"),
            name: r.get("name"),
            validity_days: r.get("validity_days"),
            default_notes: r.get("default_notes"),
        }
    }
}

backbone_core::impl_crud_repository!(QuotationTemplateRepository, QuotationTemplate, soft_delete);
