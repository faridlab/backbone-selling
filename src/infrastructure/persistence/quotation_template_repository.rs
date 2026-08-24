//! Repository for QuotationTemplate entities
//!
//! Hand-authored and declared `user_owned` in `metaphor.codegen.yaml` (the generator skips this
//! path wholesale). Holds the hand-written template SQL: the company-scoped find/list the write
//! service uses when stamping a new quotation, and the guarded insert the guarded route backs.
//! (4-layer rule: services orchestrate and own the unit of work, repositories hold the SQL.)
//!
//! Thin newtype over `backbone_orm::GenericCrudRepository<QuotationTemplate, backbone_orm::SoftDelete>`.
//! All standard CRUD methods are available via `Deref`.

use anyhow::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_orm::company_scope;

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
    pub company_id: Uuid,
    pub name: String,
    pub validity_days: i32,
    pub default_notes: Option<String>,
}

/// The exact row a template insert writes.
pub struct NewQuotationTemplateRow<'a> {
    pub id: Uuid,
    pub company_id: Uuid,
    pub name: &'a str,
    pub validity_days: i32,
    pub default_notes: Option<&'a str>,
}

/// Hand-written QuotationTemplate SQL. Lives here (not in the write service) per the module's
/// 4-layer rule.
impl QuotationTemplateRepository {
    /// Read one template, company-scoped: another company's template id simply isn't found.
    pub async fn find_template(
        &self,
        pool: &PgPool,
        template_id: Uuid,
        company_id: Uuid,
    ) -> Result<Option<QuotationTemplateRow>, sqlx::Error> {
        let row = company_scope::fetch_optional_row_scoped(
            pool,
            sqlx::query(
                r#"SELECT id, company_id, name, validity_days, default_notes
                   FROM selling.quotation_templates
                   WHERE id=$1 AND company_id=$2 AND (metadata->>'deleted_at') IS NULL"#,
            )
            .bind(template_id)
            .bind(company_id),
        )
        .await?;
        Ok(row.map(|r| QuotationTemplateRow {
            id: r.get("id"),
            company_id: r.get("company_id"),
            name: r.get("name"),
            validity_days: r.get("validity_days"),
            default_notes: r.get("default_notes"),
        }))
    }

    /// List a company's templates (name order), company-scoped.
    pub async fn list_templates(
        &self,
        pool: &PgPool,
        company_id: Uuid,
    ) -> Result<Vec<QuotationTemplateRow>, sqlx::Error> {
        let rows = company_scope::fetch_all_rows_scoped(
            pool,
            sqlx::query(
                r#"SELECT id, company_id, name, validity_days, default_notes
                   FROM selling.quotation_templates
                   WHERE company_id=$1 AND (metadata->>'deleted_at') IS NULL
                   ORDER BY name"#,
            )
            .bind(company_id),
        )
        .await?;
        Ok(rows
            .iter()
            .map(|r| QuotationTemplateRow {
                id: r.get("id"),
                company_id: r.get("company_id"),
                name: r.get("name"),
                validity_days: r.get("validity_days"),
                default_notes: r.get("default_notes"),
            })
            .collect())
    }

    /// Insert one template. Returns the raw `sqlx::Error` deliberately: the caller inspects it for
    /// a unique violation on (company_id, name) to turn it into a domain error.
    pub async fn insert_template(
        &self,
        pool: &PgPool,
        t: &NewQuotationTemplateRow<'_>,
    ) -> Result<(), sqlx::Error> {
        company_scope::execute_scoped(
            pool,
            sqlx::query(
                r#"INSERT INTO selling.quotation_templates (id, company_id, name, validity_days, default_notes)
                   VALUES ($1,$2,$3,$4,$5)"#,
            )
            .bind(t.id)
            .bind(t.company_id)
            .bind(t.name)
            .bind(t.validity_days)
            .bind(t.default_notes),
        )
        .await?;
        Ok(())
    }
}

backbone_core::impl_crud_repository!(QuotationTemplateRepository, QuotationTemplate, soft_delete);
