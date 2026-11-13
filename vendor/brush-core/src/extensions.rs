//! Definition of shell behavior traits and defaults.

use crate::{Shell, error, extensions};

/// Trait for static shell extensions. Collects all associated types needed to
/// instantiate a shell into a single containing struct.
pub trait ShellExtensions: Clone + Default + Send + Sync + 'static {
    /// Type of the error behavior implementation.
    type ErrorFormatter: ErrorFormatter;
}

/// Shell extensions implementation constructed from component types.
#[derive(Clone, Default)]
pub struct ShellExtensionsImpl<EF: ErrorFormatter = DefaultErrorFormatter> {
    _marker: std::marker::PhantomData<EF>,
}

impl<EF: ErrorFormatter> ShellExtensions for ShellExtensionsImpl<EF> {
    type ErrorFormatter = EF;
}

/// Default shell extensions implementation.
/// This is a type alias for the most common shell configuration.
pub type DefaultShellExtensions = ShellExtensionsImpl<DefaultErrorFormatter>;

/// Trait for defining shell error behaviors.
pub trait ErrorFormatter: Clone + Default + Send + Sync + 'static {
    /// Format the given error for display within the context of the provided shell.
    ///
    /// # Arguments
    ///
    /// * `error` - The error to format
    /// * `shell` - The shell context in which the error occurred.
    fn format_error(
        &self,
        error: &error::Error,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> String {
        let _ = shell;
        std::format!("error: {error:#}\n")
    }
}

/// Default shell error behavior implementation.
#[derive(Clone, Default)]
pub struct DefaultErrorFormatter;

impl ErrorFormatter for DefaultErrorFormatter {
    /// A missing command is reported as Bash does (`bash: NAME: command not found`), so tools
    /// that recognise Bash's diagnostic treat both shells alike; other errors keep the generic form.
    fn format_error(
        &self,
        error: &error::Error,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> String {
        match error.kind() {
            error::ErrorKind::CommandNotFound(name) => {
                let shell_name = shell.current_shell_name();
                let shell_name = shell_name.as_deref().unwrap_or("bash");
                std::format!("{shell_name}: {name}: command not found\n")
            }
            _ => std::format!("error: {error:#}\n"),
        }
    }
}

/// Trait for placeholder behavior (stub for future extension).
pub trait PlaceholderBehavior: Clone + Default + Send + Sync + 'static {}

/// Default placeholder implementation.
#[derive(Clone, Default)]
pub struct DefaultPlaceholder;

impl PlaceholderBehavior for DefaultPlaceholder {}
