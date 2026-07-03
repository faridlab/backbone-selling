use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "sales_order_status", rename_all = "snake_case")]
pub enum SalesOrderStatus {
    Draft,
    ToDeliver,
    ToBill,
    ToDeliverAndBill,
    Completed,
    Closed,
    Cancelled,
}

impl std::fmt::Display for SalesOrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::ToDeliver => write!(f, "to_deliver"),
            Self::ToBill => write!(f, "to_bill"),
            Self::ToDeliverAndBill => write!(f, "to_deliver_and_bill"),
            Self::Completed => write!(f, "completed"),
            Self::Closed => write!(f, "closed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for SalesOrderStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "to_deliver" => Ok(Self::ToDeliver),
            "to_bill" => Ok(Self::ToBill),
            "to_deliver_and_bill" => Ok(Self::ToDeliverAndBill),
            "completed" => Ok(Self::Completed),
            "closed" => Ok(Self::Closed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("Unknown SalesOrderStatus variant: {}", s)),
        }
    }
}

impl Default for SalesOrderStatus {
    fn default() -> Self {
        Self::Draft
    }
}
