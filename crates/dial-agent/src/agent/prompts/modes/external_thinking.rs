//! Private-scratchpad instruction; filtering remains in the mode runtime.

/// System-prompt addition that routes private reasoning into one `<scratchpad>` block.
pub const INSTRUCTION: &str = "Reason privately before answering. Put all scratch reasoning inside one <scratchpad>...</scratchpad> block at the start of your reply. Text outside that block is shown to the user.";
