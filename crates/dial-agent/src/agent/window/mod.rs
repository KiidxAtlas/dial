//! Context window: context files and meter, projection, prompt assembly and output schemas.

pub mod context;
pub mod context_breakdown;
pub(in crate::agent) mod context_meter;
pub(in crate::agent) mod context_notes;
pub mod output_schema;
pub(in crate::agent) mod projection;
pub mod prompt;
pub(in crate::agent) mod prompt_template;
pub(in crate::agent) mod request_prefix;
