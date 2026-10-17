//! Host ↔ worker JSON protocol for the computer-use Node worker (line-delimited JSON over stdio).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-call session state the worker needs for capture and coordinate handling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerSessionSnapshot {
    pub cwd: String,
    pub session_id: String,
    pub capture_max_width: u32,
    pub capture_max_height: u32,
    pub display: String,
    pub read_only: bool,
}

/// Text or image content emitted by a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ComputerContent {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Image {
        data: String,
        mime_type: String,
    },
}

/// Screenshot metadata written to a temp PNG by the native helper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerScreenshot {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub coordinate_width: u32,
    pub coordinate_height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_height: Option<u32>,
    pub target: String,
}

/// Successful run payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRunOk {
    #[serde(default)]
    pub displays: Vec<ComputerContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_value: Option<Value>,
    #[serde(default)]
    pub screenshots: Vec<ComputerScreenshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}
