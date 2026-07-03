use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "quotation_status", rename_all = "snake_case")]
pub enum QuotationStatus {
    Draft,
    Sent,
    Accepted,
    Ordered,
    Rejected,
    Expired,
    Cancelled,
}

impl std::fmt::Display for QuotationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Sent => write!(f, "sent"),
            Self::Accepted => write!(f, "accepted"),
            Self::Ordered => write!(f, "ordered"),
            Self::Rejected => write!(f, "rejected"),
            Self::Expired => write!(f, "expired"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for QuotationStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "sent" => Ok(Self::Sent),
            "accepted" => Ok(Self::Accepted),
            "ordered" => Ok(Self::Ordered),
            "rejected" => Ok(Self::Rejected),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("Unknown QuotationStatus variant: {}", s)),
        }
    }
}

impl Default for QuotationStatus {
    fn default() -> Self {
        Self::Draft
    }
}
