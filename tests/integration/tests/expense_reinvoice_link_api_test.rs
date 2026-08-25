//! ExpenseReinvoiceLink API Integration Tests
//!
//! Hand-added (the harness is user-owned/frozen): the reinvoice link's generic-CRUD section,
//! mirroring the sales_team pattern. Hits the live server's generic CRUD router
//! (`/api/v1/expense_reinvoice_links`); skips when API_BASE_URL is unset. The VALIDATED verbs
//! (attach/mark-invoiced/list, the double-bill guards) are proven in `tests/reinvoice_link.rs`.

use serde_json::{json, Value};
use uuid::Uuid;

use super::crud_test_base::{CrudTestConfig, GenericCrudTest, TestDataGenerator};
use crate::integration::helpers::CommonUtils;

/// Test data generator for ExpenseReinvoiceLink
pub struct ExpenseReinvoiceLinkTestData;

impl TestDataGenerator for ExpenseReinvoiceLinkTestData {
    fn generate_create_payload(&self, _utils: &CommonUtils) -> Value {
        json!({
            "company_id": Uuid::new_v4().to_string(),
            "order_id": Uuid::new_v4().to_string(),
            "expense_id": Uuid::new_v4().to_string(),
            "amount": "150000.00",
            "state": "pending",
            "metadata": json!({}),
        })
    }

    fn generate_update_payload(&self, _id: &str, _utils: &CommonUtils) -> Value {
        json!({
            "company_id": Uuid::new_v4().to_string(),
            "order_id": Uuid::new_v4().to_string(),
            "expense_id": Uuid::new_v4().to_string(),
            "amount": "175000.00",
            "state": "invoiced",
            "metadata": json!({}),
        })
    }

    fn generate_invalid_payload(&self) -> Value {
        json!({
            // Missing required fields (company_id, order_id, expense_id, amount)
            "metadata": json!({}),
        })
    }
}

/// ExpenseReinvoiceLink API test suite
pub struct ExpenseReinvoiceLinkApiTest {
    inner: GenericCrudTest<ExpenseReinvoiceLinkTestData>,
}

impl ExpenseReinvoiceLinkApiTest {
    pub fn new() -> Self {
        let mut config = CrudTestConfig::new("/api/v1/expense_reinvoice_links", "ExpenseReinvoiceLink");
        config.supports_soft_delete = true;
        Self {
            inner: GenericCrudTest::new(config, ExpenseReinvoiceLinkTestData),
        }
    }

    pub async fn run_all(&mut self) -> Vec<crate::integration::framework::TestResult> {
        self.inner.run_all().await
    }
}

impl Default for ExpenseReinvoiceLinkApiTest {
    fn default() -> Self {
        Self::new()
    }
}
