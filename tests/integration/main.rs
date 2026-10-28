//! Integration tests for the `dial` binary and public library surface.
//!
//! One test binary: each integration target links the whole library again, so the
//! suites share a single link. Run one suite with `cargo test --test integration <module>::`.

mod agent;
mod architecture;
mod automatic;
mod benchmark;
mod build_info;
mod cli_print;
mod hashline;
mod isolation;
mod proposals;
mod review;
mod shell_sessions;
mod ui_configuration;
