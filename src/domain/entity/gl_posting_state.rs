use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "gl_posting_state", rename_all = "snake_case")]
pub enum GlPostingState {
    Pending,
    Posted,
    Failed,
}

impl std::fmt::Display for GlPostingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Posted => write!(f, "posted"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl FromStr for GlPostingState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "posted" => Ok(Self::Posted),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("Unknown GlPostingState variant: {}", s)),
        }
    }
}

impl Default for GlPostingState {
    fn default() -> Self {
        Self::Pending
    }
}
