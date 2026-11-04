// Commands are positional tuples, not records with a `name:` field.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help"),
    ("/settings", "Open settings"),
    ("/tree", "Browse the session tree"),
    ("/label", "Label the current answer"),
    ("/compact", "Compact the session"),
];
