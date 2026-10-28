//! IDA Pro runtime setting declarations (the `ida` tool).
use super::*;
pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "ida.python",
        ida.python,
        TO,
        "IDA",
        "Python interpreter",
        "Python executable that can import idapro (idalib) from your IDA Pro install. Empty uses python3 from PATH.",
        Kind::Text,
    ),
    field!(
        "ida.idadir",
        ida.idadir,
        TO,
        "IDA",
        "IDA install directory",
        "IDA Pro installation directory exported to the worker as IDADIR. Empty inherits IDADIR from the environment.",
        Kind::Text,
    ),
    field!(
        "ida.timeout_secs",
        ida.timeout_secs,
        TO,
        "IDA",
        "Edit and exec timeout",
        "Seconds before an IDA edit or exec script is interrupted. Opening a database has its own longer limit.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 10
        },
    ),
];
