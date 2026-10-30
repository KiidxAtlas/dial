//! The overlay run loop: input, redraw and the busy and wait flows.

use crate::app::discovery::shared;
use crate::app::improve_view::improve_view;
use crate::app::overlay::ActiveOverlay;
use crate::app::overlay::compute::ComputeMonitor;
use crate::app::overlay::env::OverlayEnv;
use crate::app::overlay::settings::settings_embedding_models;
use crate::app::shell::TICK;
use crate::config::types::Config;
use crate::llm::model_config::{ModelSelection, UiConfig};
use crate::ui::input::{poll, read};
use crate::ui::overlay::DrawCtx;
use crate::ui::status::Phase;
use crate::ui::statusline;
use crate::ui::term::OverlayTerminal;
use anyhow::Result;
use crossterm::event::Event;
use std::time::{Duration, Instant};

/// What a blocking wait (compute lock, local preparation) does after one terminal event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum WaitFlow {
    /// Keep waiting.
    Continue,
    /// The configured interrupt (or an explicit exit) abandons the wait.
    Cancel,
    /// A submitted message must run after the wait, so the wait ends without its work.
    Stop,
}

/// Owned panel state lets agent callbacks keep draining while an overlay is visible.
pub(in crate::app) struct BusyOverlay {
    pub(in crate::app) overlay: ActiveOverlay,
    pub(in crate::app) paste_guard: crate::ui::input::PasteGuard,
    pub(in crate::app) compute_monitor: Option<ComputeMonitor>,
    pub(in crate::app) verify_lora: Option<ModelSelection>,
    pub(in crate::app) latest: Option<Config>,
    pub(in crate::app) next: Option<String>,
    pub(in crate::app) seen_generation: u64,
}

pub(in crate::app) fn refresh_overlay(env: &mut OverlayEnv<'_>, overlay: &mut ActiveOverlay) {
    if let Some(finished) = crate::app::commands::extensions_browser::step(
        env.extensions_pending,
        env.extensions_job,
        env.app,
    ) {
        crate::app::commands::extensions_browser::finish(finished, |changes, scope| {
            env.save(changes, scope).map(|_| ())
        });
    }
    if let ActiveOverlay::Palette(palette) = overlay {
        palette.set_busy(env.status.phase != Phase::Idle);
    }
    if let ActiveOverlay::Improve(panel) = &mut *overlay
        && panel.wants_refresh()
    {
        panel.update(improve_view(env.app));
    }
    if let (Some(monitor), ActiveOverlay::Compute(panel)) = (&env.compute_monitor, &mut *overlay) {
        if let Some(value) = shared(&monitor.snapshot).take() {
            panel.update(value);
        }
        if let Some((message, is_error)) = shared(&monitor.message).take() {
            panel.banner(message, is_error);
        }
    }
    if let ActiveOverlay::Extensions(panel) = overlay {
        panel.tick();
        if panel.take_refresh()
            && let Ok(config) = env.app.reload()
        {
            crate::app::commands::extensions_browser::refresh(panel, env.app.layout(), &config);
        } else if let Some(query) = panel.catalog_search_due(Instant::now()) {
            match env.app.reload() {
                Ok(config) => crate::app::commands::extensions_browser::fetch_catalog(
                    panel,
                    env.app.layout(),
                    &config,
                    crate::ui::overlay::extensions::model::Tab::Mcp,
                    query,
                ),
                Err(error) => panel.banner(format!("could not read settings: {error:#}"), true),
            }
        }
    }
    if let ActiveOverlay::Models(models) = &mut *overlay
        && models.take_refresh()
    {
        env.catalog.refresh(env.app);
    }
    if let ActiveOverlay::Subagents(hub) = &mut *overlay {
        hub.refresh(
            &env.app.layout,
            std::time::Instant::now(),
            env.runtime,
            &env.app.config.role_statuses(),
            env.live_tasks,
        );
    }
    if let ActiveOverlay::SubagentViewer(viewer) = &mut *overlay {
        viewer.refresh(
            std::time::Instant::now(),
            &env.app.layout,
            env.runtime,
            env.live_tasks,
        );
    }
    if let (Some(background), ActiveOverlay::Jobs(panel)) = (env.background_control, &mut *overlay)
    {
        let attached = panel.attached().map(str::to_owned);
        if let Some(view) = background.jobs_view(attached.as_deref()) {
            panel.set_jobs(
                view.jobs
                    .into_iter()
                    .map(|job| crate::ui::overlay::jobs::JobRow {
                        name: job.name,
                        command: job.command,
                        status: job.status,
                        running: job.running,
                        terminal: job.terminal,
                        mode: "session".into(),
                    })
                    .collect(),
            );
            panel.set_screen(view.screen, view.cursor);
        }
    }
    if let ActiveOverlay::Git(git) = overlay {
        git.tick();
    }
    env.refresh_setup(overlay);
    env.collect_auth(overlay);
    env.catalog.collect();
    if env.catalog.generation != env.seen_generation {
        env.seen_generation = env.catalog.generation;
        if let Some(discovery) = env.catalog.get() {
            match overlay {
                ActiveOverlay::Models(models) => models.set_discovery(discovery),
                ActiveOverlay::Providers(providers) => providers.set_discovery(discovery),
                ActiveOverlay::Settings(settings) => {
                    settings.set_embedding_models(settings_embedding_models(Some(&discovery)))
                }
                _ => {}
            }
        }
    }
    env.themes.reload_if_changed();
}

pub(in crate::app) fn draw_overlay_frame(
    frame: &mut ratatui::Frame<'_>,
    env: &OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
) -> Vec<crate::ui::images::Placement> {
    let themes = &*env.themes;
    let preview = |cfg: &UiConfig, width: u16| {
        statusline::preview_row(cfg, env.status, themes.current(), width)
    };
    let ctx = DrawCtx {
        theme: themes.current(),
        keys: env.keys,
        status: env.status,
        preview_status: &preview,
    };
    overlay.draw(frame, &ctx);
    overlay.native_placements()
}

pub(in crate::app) fn overlay_input(
    env: &mut OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
    paste_guard: &mut crate::ui::input::PasteGuard,
    mut event: Event,
) -> bool {
    if matches!(overlay, ActiveOverlay::Palette(_)) {
        match &event {
            Event::Paste(_) => paste_guard.pasted(Instant::now()),
            Event::Key(key) => {
                if let Some(value) = paste_guard.text_for_key(
                    *key,
                    poll(Duration::ZERO).unwrap_or(false),
                    Instant::now(),
                ) {
                    if value.is_empty() {
                        return false;
                    }
                    event = Event::Paste(value.into());
                }
            }
            _ => {}
        }
    }
    if matches!(event, Event::Resize(..)) {
        return false;
    }
    let outcome = overlay.handle(&event, env.keys);
    env.apply(outcome, overlay)
}

pub(in crate::app) fn run_overlay(
    ot: &mut OverlayTerminal<'_>,
    env: &mut OverlayEnv<'_>,
    overlay: &mut ActiveOverlay,
) -> Result<()> {
    let mut paste_guard = crate::ui::input::PasteGuard::default();
    // Every panel unrolls from the top over a short reveal when it opens, then stays still.
    let opened = Instant::now();
    let motion = crate::ui::fx::motion(env.app.config.ui.motion, env.themes.current());
    loop {
        refresh_overlay(env, overlay);
        let progress = if motion {
            crate::ui::fx::progress(opened.elapsed(), crate::ui::fx::REVEAL)
        } else {
            1.0
        };
        ot.draw(|frame| {
            let placements = draw_overlay_frame(frame, env, overlay);
            if progress < 1.0 {
                let area = frame.area();
                crate::ui::fx::reveal(frame.buffer_mut(), area, progress);
                return Vec::new();
            }
            placements
        })?;
        let wait = if progress < 1.0 {
            crate::ui::fx::FRAME
        } else {
            TICK
        };
        if !poll(wait)? {
            continue;
        }
        if overlay_input(env, overlay, &mut paste_guard, read()?) {
            return Ok(());
        }
    }
}
