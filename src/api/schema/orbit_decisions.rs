//! The Orbit decision layer: routing suggestions and the decision journal.
use serde::{Deserialize, Serialize};

/// Asks which team role should own a request. Read-only: nothing is assigned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitRouteParams {
    /// The request to route, at most 4000 bytes.
    pub request: String,
}

/// Lists recent decisions; with `check`, tests the provider's key and endpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitDecisionsParams {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
}
