//! Controller-owned integration lifecycle facts. None of these values are remote tool payload.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionHealth {
    Connected,
    Connecting,
    #[default]
    Disconnected,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationHealth {
    Healthy,
    Unsupported,
    Degraded,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFreshness {
    Fresh,
    Stale,
    #[default]
    Unknown,
}

/// Independent channel/catalog states and a monotonically increasing local generation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationHealth {
    pub connection: ConnectionHealth,
    pub notifications: NotificationHealth,
    pub catalog: CatalogFreshness,
    pub generation: u64,
    pub transitions: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    NotDispatched,
    Dispatched,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    Reply,
    Cancelled,
    TimedOut,
    TransportLost,
    #[default]
    NotStarted,
}

/// Low-cardinality failure categories; never credentials, endpoints or arbitrary server prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationFailure {
    Admission,
    Authentication,
    Connection,
    Protocol,
    Remote,
    Cancelled,
    Timeout,
}

/// One logical call's settlement plus the session health observed at its boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationExecution {
    pub health: IntegrationHealth,
    pub state_changes: u64,
    pub dispatch: DispatchState,
    pub settlement: Settlement,
    pub failure: Option<IntegrationFailure>,
}
