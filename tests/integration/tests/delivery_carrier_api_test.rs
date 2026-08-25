//! DeliveryCarrier API Integration Tests
//!
//! Hand-added (the harness is user-owned/frozen): the carrier registry's generic-CRUD section,
//! mirroring the sales_team pattern. Hits the live server's generic CRUD router
//! (`/api/v1/delivery_carriers`); skips when API_BASE_URL is unset. The VALIDATED registry verbs
//! (duplicate-name refusal, deactivate-not-delete) are proven in `tests/carrier_registry.rs`.

use serde_json::{json, Value};
use uuid::Uuid;

use super::crud_test_base::{CrudTestConfig, GenericCrudTest, TestDataGenerator};
use crate::integration::helpers::CommonUtils;

/// Test data generator for DeliveryCarrier
pub struct DeliveryCarrierTestData;

impl TestDataGenerator for DeliveryCarrierTestData {
    fn generate_create_payload(&self, _utils: &CommonUtils) -> Value {
        json!({
            "company_id": Uuid::new_v4().to_string(),
            "name": format!("Carrier {}", &Uuid::new_v4().simple().to_string()[..8]),
            "active": true,
            "tracking_url_template": "https://track.example.com/{tracking_ref}",
            "metadata": json!({}),
        })
    }

    fn generate_update_payload(&self, _id: &str, _utils: &CommonUtils) -> Value {
        json!({
            "company_id": Uuid::new_v4().to_string(),
            "name": format!("Carrier {}", &Uuid::new_v4().simple().to_string()[..8]),
            "active": false,
            "tracking_url_template": null,
            "metadata": json!({}),
        })
    }

    fn generate_invalid_payload(&self) -> Value {
        json!({
            // Missing required fields (company_id, name, active)
            "metadata": json!({}),
        })
    }
}

/// DeliveryCarrier API test suite
pub struct DeliveryCarrierApiTest {
    inner: GenericCrudTest<DeliveryCarrierTestData>,
}

impl DeliveryCarrierApiTest {
    pub fn new() -> Self {
        let mut config = CrudTestConfig::new("/api/v1/delivery_carriers", "DeliveryCarrier");
        config.supports_soft_delete = true;
        Self {
            inner: GenericCrudTest::new(config, DeliveryCarrierTestData),
        }
    }

    pub async fn run_all(&mut self) -> Vec<crate::integration::framework::TestResult> {
        self.inner.run_all().await
    }
}

impl Default for DeliveryCarrierApiTest {
    fn default() -> Self {
        Self::new()
    }
}
