//! Real shell state with an in-memory terminal and isolated settings; no model or TTY required.
use super::Shell;
use crate::app::core::App;
use crate::ui::term;

pub(super) fn shell() -> (tempfile::TempDir, tempfile::TempDir, App, Shell) {
    let (home, project, layout, _) = crate::app::tests::config();
    crate::config::settings_store::save_config(
        &layout,
        crate::config::settings_store::Scope::Project,
        &[crate::config::settings_store::Change::Set {
            path: "sandbox.allow_network".into(),
            value: false.into(),
        }],
    )
    .unwrap();
    let config = crate::config::types::Config::load(&layout, None).unwrap();
    let app = App::new(
        layout,
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    let shell = Shell::with_terminal(
        &app,
        String::new(),
        term::with_writer(Box::new(std::io::sink()), (100, 30)),
    )
    .unwrap();
    // These tests exercise panel routing, not provider discovery. Cache an empty catalog so
    // opening Providers/Models cannot probe the network or use the operator's environment.
    shell.catalog.slot.lock().unwrap().1 = Some(Default::default());
    (home, project, app, shell)
}
