//! Identity of the executable being used, independent of the current checkout.
use serde_json::{Value, json};

pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("DIAL_BUILD_REVISION"),
    "; dirty=",
    env!("DIAL_BUILD_DIRTY"),
    ")"
);

/// The commit this executable was built from (a Git hash prefix), or `unknown`.
pub const REVISION: &str = env!("DIAL_BUILD_REVISION");
/// Whether the build inputs differed from [`REVISION`]: `true`, `false` or `unknown`.
pub const DIRTY: &str = env!("DIAL_BUILD_DIRTY");

pub fn report() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "revision": env!("DIAL_BUILD_REVISION"),
        "dirty_at_build": env!("DIAL_BUILD_DIRTY"),
        "built_at_unix": env!("DIAL_BUILD_EPOCH"),
        "target": env!("DIAL_BUILD_TARGET"),
        "profile": env!("DIAL_BUILD_PROFILE"),
        "executable": std::env::current_exe().ok().map(|path| path.to_string_lossy().into_owned()),
    })
}
