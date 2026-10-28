use super::*;
use crate::ui::status::Phase;
use ratatui::style::Color;

fn data() -> StatusData {
    StatusData {
        provider: "mlx".into(),
        model: "qwen3-9b".into(),
        project_path: "/work/projects/dial".into(),
        branch: "main".into(),
        git_counts: Some((1, 2, 3)),
        prompt_tokens: Some(3900),
        context_window: 32_000,
        ..StatusData::default()
    }
}
fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
fn render(cfg: &UiConfig, data: &StatusData, width: u16) -> Line<'static> {
    border_line(cfg, data, &Theme::dark(), width, Style::default(), "╭", "╮")
}
fn cfg_with(status: StatusPreset) -> UiConfig {
    UiConfig {
        status,
        ..UiConfig::default()
    }
}
const ALL_PRESETS: [StatusPreset; 7] = [
    StatusPreset::Default,
    StatusPreset::Minimal,
    StatusPreset::Compact,
    StatusPreset::Full,
    StatusPreset::Nerd,
    StatusPreset::Ascii,
    StatusPreset::Custom,
];
const ALL_SEPARATORS: [StatusSeparator; 7] = [
    StatusSeparator::Powerline,
    StatusSeparator::PowerlineThin,
    StatusSeparator::Slash,
    StatusSeparator::Pipe,
    StatusSeparator::Block,
    StatusSeparator::None,
    StatusSeparator::Ascii,
];

#[test]
fn settings_offers_exactly_the_segments_the_renderer_understands() {
    for name in crate::config::schema::STATUS_SEGMENTS {
        assert_eq!(
            canonical(name).map(|(canonical, _)| canonical),
            Some(*name),
            "{name}"
        );
    }
    let offered: std::collections::BTreeSet<_> =
        crate::config::schema::STATUS_SEGMENTS.iter().collect();
    assert_eq!(offered.len(), crate::config::schema::STATUS_SEGMENTS.len());
}

#[test]
fn custom_thinking_segment_is_not_hidden_when_inference_is_not_requested() {
    let status = StatusData {
        thinking: "high".into(),
        inference: "temp 0.2 · think on · out 4096".into(),
        ..data()
    };
    let cfg = UiConfig {
        status_left: vec!["thinking".into()],
        status_right: vec![],
        ..cfg_with(StatusPreset::Custom)
    };
    assert!(plain(&render(&cfg, &status, 80)).contains("THINK HIGH"));
}

#[test]
fn local_request_settings_segment_is_live_optional_and_width_safe() {
    let mut status = data();
    let cfg = UiConfig {
        status_left: vec!["model".into(), "inference".into()],
        status_right: vec![],
        ..cfg_with(StatusPreset::Custom)
    };
    let initial = plain(&render(&cfg, &status, 160));
    status.inference = "temp 0.6 · think on · out 4096".into();
    let active = plain(&render(&cfg, &status, 160));
    assert_ne!(initial, active);
    assert!(active.contains(&status.inference));
    for width in 8..160 {
        assert_eq!(render(&cfg, &status, width).width(), usize::from(width));
    }
    status.inference = "temp server default · think off · out 2048".into();
    assert!(plain(&render(&cfg, &status, 160)).contains(&status.inference));
}

#[test]
fn activity_row_fits_all_widths_and_preserves_silence_and_custom_preferences() {
    let mut status = StatusData {
        phase: Phase::Working,
        activity: "running 日本語 tool".into(),
        elapsed_secs: 65.0,
        ..data()
    };
    for theme in [
        Theme::dark(),
        Theme::light(),
        Theme::mono(),
        Theme::dark().with_glyphs(Glyphs::Ascii),
    ] {
        for preset in ALL_PRESETS {
            let cfg = cfg_with(preset);
            for width in 8..160 {
                if let Some(row) = activity_row(&cfg, &status, &theme, width) {
                    assert_eq!(row.width(), usize::from(width));
                }
            }
        }
    }
    status.model_progress = Some(crate::llm::ModelProgress::new(
        crate::llm::InferencePhase::Processing,
    ));
    status.model_silence_secs = 27;
    let row = activity_row(&UiConfig::default(), &status, &Theme::dark(), 40).unwrap();
    assert!(plain(&row).contains("no progress for 27s"));
    status.activity = "stopping".into();
    let row = activity_row(&UiConfig::default(), &status, &Theme::dark(), 40).unwrap();
    assert!(plain(&row).contains("stopping"));
    assert!(!plain(&row).contains("processing"));
    let custom = UiConfig {
        status_left: vec!["model".into()],
        status_right: vec![],
        ..cfg_with(StatusPreset::Custom)
    };
    assert!(activity_row(&custom, &status, &Theme::dark(), 80).is_none());
    status.phase = Phase::Idle;
    assert!(activity_row(&UiConfig::default(), &status, &Theme::dark(), 80).is_none());
}

#[test]
fn local_progress_shows_stages_live_tokens_and_rates_in_the_default_composer() {
    use crate::llm::{InferencePhase, ModelProgress};
    let mut progress = ModelProgress::new(InferencePhase::Loading);
    let mut data = StatusData {
        phase: Phase::Working,
        model_progress: Some(progress.clone()),
        ..data()
    };
    assert!(plain(&render(&UiConfig::default(), &data, 110)).contains("loading model"));
    progress.phase = InferencePhase::Processing;
    progress.prompt_tokens = Some(6000);
    progress.processed_tokens = Some(2048);
    data.model_progress = Some(progress.clone());
    assert!(plain(&render(&UiConfig::default(), &data, 110)).contains("2048/6000 tokens (34%)"));
    progress.phase = InferencePhase::Generating;
    progress.output_tokens = Some(128);
    progress.tokens_per_second = Some(31.2);
    data.model_progress = Some(progress.clone());
    assert!(
        plain(&render(&UiConfig::default(), &data, 110))
            .contains("generating 128 tokens · 31.2 tok/s")
    );
    for width in [60, 80] {
        let row = plain(&render(&UiConfig::default(), &data, width));
        assert!(row.contains("generating 128"), "width {width}: {row}");
    }
    for width in 1..140 {
        assert_eq!(
            render(&UiConfig::default(), &data, width).width(),
            width as usize
        );
    }
    data.output_tokens = 100;
    assert_eq!(data.live_output_tokens(), (228, false));
    progress.estimated = true;
    data.model_progress = Some(progress);
    assert!(
        plain(&render(&UiConfig::default(), &data, 110))
            .contains("generating ~128 tokens · ~31.2 tok/s")
    );
    data.model_progress = None;
    assert_eq!(data.live_output_tokens(), (100, false));
}

#[test]
fn thinking_is_visible_in_presets_and_fits_narrow_terminals() {
    for preset in ALL_PRESETS {
        let cfg = UiConfig {
            status_left: vec!["model".into(), "reasoning".into()],
            ..cfg_with(preset)
        };
        for level in ["high", "off", "on", "default", "unsupported"] {
            let data = StatusData {
                thinking: level.into(),
                ..data()
            };
            assert!(
                plain(&render(&cfg, &data, 200))
                    .contains(&format!("THINK {}", level.to_uppercase()))
            );
            for width in 1..100 {
                assert_eq!(render(&cfg, &data, width).width(), width as usize);
            }
        }
        assert!(!plain(&render(&cfg, &data(), 200)).contains("THINK "));
    }
}

#[test]
fn the_model_segment_reports_lora_state() {
    use crate::ui::status::Lora;
    let row = |lora| {
        plain(&render(
            &UiConfig::default(),
            &StatusData { lora, ..data() },
            100,
        ))
    };
    assert!(
        row(Lora::Active(3)).contains("LoRA accepted v3"),
        "{}",
        row(Lora::Active(3))
    );
    assert!(row(Lora::Ready).contains("LoRA awaiting training"));
    for state in [
        Lora::Training,
        Lora::Paused,
        Lora::Evaluating,
        Lora::Loading,
        Lora::Loaded,
        Lora::Used,
        Lora::Failed,
    ] {
        assert!(row(state).contains(&state.label()), "{}", row(state));
    }
    assert!(row(Lora::Off).contains("LoRA off"));
    assert!(!row(Lora::Unsupported).contains("LoRA"));
    assert!(
        !row(Lora::Unknown).contains("LoRA"),
        "unknown is not guessed"
    );
}

fn busy_full() -> StatusData {
    StatusData {
        role: "subagent".into(),
        approval: "ask".into(),
        sandbox: "workspace".into(),
        version: Some(3),
        learning: "idle".into(),
        output_tokens: 1200,
        tokens_per_second: Some(27.54),
        phase: Phase::Working,
        elapsed_secs: 12.4,
        tick: 3,
        session: "abcdef123456".into(),
        clock: "10:54".into(),
        ..data()
    }
}

#[test]
fn presets_map_to_their_segment_lists_and_custom_uses_the_configured_ones() {
    let names = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        segments(&cfg_with(StatusPreset::Default)),
        (
            names(&[
                "spinner",
                "model",
                "mode",
                "thinking",
                "inference",
                "path",
                "git"
            ]),
            names(&["workers", "context"])
        )
    );
    assert_eq!(
        segments(&cfg_with(StatusPreset::Minimal)),
        (
            names(&["model", "thinking", "inference"]),
            names(&["context"])
        )
    );
    assert_eq!(
        segments(&cfg_with(StatusPreset::Compact)),
        (
            names(&["spinner", "model", "thinking", "inference", "git"]),
            names(&["workers", "context", "tokens"])
        )
    );
    assert_eq!(
        segments(&cfg_with(StatusPreset::Full)),
        (
            names(&[
                "spinner",
                "model",
                "thinking",
                "inference",
                "role",
                "path",
                "git",
                "approval",
                "sandbox"
            ]),
            names(&["workers", "tokens", "rate", "learning", "context"])
        )
    );
    assert_eq!(
        segments(&cfg_with(StatusPreset::Nerd)),
        segments(&cfg_with(StatusPreset::Default))
    );
    assert_eq!(
        segments(&cfg_with(StatusPreset::Ascii)),
        segments(&cfg_with(StatusPreset::Default))
    );
    let custom = UiConfig {
        status: StatusPreset::Custom,
        status_left: names(&["git", "bogus"]),
        status_right: names(&["time"]),
        ..UiConfig::default()
    };
    assert_eq!(
        segments(&custom),
        (names(&["git", "bogus"]), names(&["time"]))
    );
}

#[test]
fn the_line_is_exactly_the_requested_width_for_every_preset_separator_and_width() {
    std::thread::scope(|scope| {
        for status in ALL_PRESETS {
            scope.spawn(move || {
                let theme_ascii = Theme::dark().with_glyphs(Glyphs::Ascii);
                for separator in ALL_SEPARATORS {
                    let cfg = UiConfig {
                        status,
                        status_separator: separator,
                        status_left: vec!["model".into(), "path".into()],
                        status_right: vec!["context".into()],
                        ..UiConfig::default()
                    };
                    for theme in [
                        Theme::dark(),
                        theme_ascii.clone(),
                        Theme::dark().with_glyphs(Glyphs::Nerd),
                    ] {
                        for width in 0..=130u16 {
                            for d in [data(), busy_full(), StatusData::default()] {
                                let line = border_line(
                                    &cfg,
                                    &d,
                                    &theme,
                                    width,
                                    Style::default(),
                                    "╭",
                                    "╮",
                                );
                                assert_eq!(
                                    text::line_width(&line),
                                    width as usize,
                                    "{status:?} {separator:?} width {width}: {:?}",
                                    plain(&line)
                                );
                                let row = preview_row(&cfg, &d, &theme, width);
                                assert_eq!(
                                    text::line_width(&row),
                                    width as usize,
                                    "preview {status:?} width {width}"
                                );
                            }
                        }
                    }
                }
            });
        }
    });
}

#[test]
fn default_line_shows_segments_in_order_with_the_context_on_the_right() {
    let line = plain(&render(&UiConfig::default(), &data(), 100));
    assert!(line.starts_with("╭─") && line.ends_with("─╮"), "{line:?}");
    let at = |needle: &str| {
        line.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing in {line:?}"))
    };
    assert!(
        at("mlx/qwen3-9b") < at("projects/dial")
            && at("projects/dial") < at("main +1 ~2 ?3")
            && at("main +1 ~2 ?3") < at("3.9k/32k"),
        "{line:?}"
    );
    assert!(line.contains("3.9k/32k"), "{line:?}");
    assert!(
        !line.contains("work/projects"),
        "path is abbreviated to its last two components: {line:?}"
    );
    assert!(
        line.trim_end_matches("─╮").trim_end().ends_with("3.9k/32k"),
        "context sits at the right end: {line:?}"
    );
    let full_path = UiConfig {
        status_path_abbreviate: false,
        ..UiConfig::default()
    };
    assert!(plain(&render(&full_path, &data(), 120)).contains("/work/projects/dial"));
    let no_counts = UiConfig {
        status_git_counts: false,
        ..UiConfig::default()
    };
    let line = plain(&render(&no_counts, &data(), 120));
    assert!(line.contains("main") && !line.contains("+1"), "{line:?}");
}

#[test]
fn the_least_important_segments_leave_first_and_the_model_stays_longest() {
    let cfg = UiConfig::default();
    let present = |width: u16, name: &str| plain(&render(&cfg, &data(), width)).contains(name);
    let vanishes_at = |name: &str| (1..=140u16).rev().find(|w| !present(*w, name)).unwrap_or(0);
    let (git, path, context, model) = (
        vanishes_at("main"),
        vanishes_at("projects/dial"),
        vanishes_at("3.9k/32k"),
        vanishes_at("qwen"),
    );
    assert!(
        git > path && path > context && context > model,
        "git {git} path {path} context {context} model {model}"
    );
    // Between the git and path thresholds: path and model remain, git is gone.
    let line = plain(&render(&cfg, &data(), git));
    assert!(
        line.contains("projects/dial") && line.contains("mlx/qwen3-9b") && !line.contains("main"),
        "{line:?}"
    );
    // Once an optional segment is gone it never comes back at narrower widths... or the reverse.
    for name in ["main", "projects/dial", "3.9k/32k"] {
        let mut seen_missing = false;
        for width in (20..=140u16).rev() {
            let here = present(width, name);
            assert!(!(seen_missing && here), "{name} reappears at width {width}");
            seen_missing |= !here;
        }
    }
}

#[test]
fn long_texts_are_shortened_with_an_ellipsis_never_wrapped() {
    let long = StatusData {
        model: "a-very-long-model-name-that-will-not-fit-anywhere-near".into(),
        ..data()
    };
    let line = render(&UiConfig::default(), &long, 50);
    let text = plain(&line);
    assert_eq!(text::line_width(&line), 50);
    assert!(
        text.contains("a-very") && text.contains('…') && !text.contains("mlx/"),
        "the provider goes first, then the model name shrinks with an ellipsis: {text:?}"
    );
    let ascii = border_line(
        &UiConfig::default(),
        &long,
        &Theme::dark().with_glyphs(Glyphs::Ascii),
        50,
        Style::default(),
        "+",
        "+",
    );
    assert!(
        plain(&ascii).contains("...") && plain(&ascii).is_ascii(),
        "{:?}",
        plain(&ascii)
    );
    let deep = StatusData {
        project_path: "/a/b/c/an-extraordinarily-long-directory-name".into(),
        ..data()
    };
    let cfg = UiConfig {
        status: StatusPreset::Custom,
        status_left: vec!["path".into()],
        status_right: vec![],
        ..UiConfig::default()
    };
    let text = plain(&render(&cfg, &deep, 30));
    assert!(
        text.contains("…") && text.contains("name"),
        "the tail of a path is kept: {text:?}"
    );
}

#[test]
fn unknown_facts_are_absent_and_an_unknown_context_is_never_zero() {
    let unknown = StatusData {
        prompt_tokens: None,
        ..data()
    };
    let line = plain(&render(&UiConfig::default(), &unknown, 100));
    assert!(
        line.contains(&format!("{} ctx", human(u64::from(unknown.context_window))))
            && !line.contains("0%"),
        "before the first request the window is shown, never a usage of zero: {line:?}"
    );
    let no_window = StatusData {
        context_window: 0,
        ..data()
    };
    assert!(plain(&render(&UiConfig::default(), &no_window, 100)).contains("ctx ?"));
    let full = cfg_with(StatusPreset::Full);
    let line = plain(&render(&full, &data(), 160));
    for absent in ["tok/s", "learning", "approval", "sandbox", "v0", "↓0"] {
        assert!(
            !line.contains(absent),
            "{absent:?} must not appear when unknown: {line:?}"
        );
    }
    let nothing = plain(&render(&UiConfig::default(), &StatusData::default(), 80));
    assert!(
        nothing.contains("ctx ?") && !nothing.contains("mlx"),
        "{nothing:?}"
    );
}

#[test]
fn full_preset_shows_every_known_segment() {
    let line = plain(&render(&cfg_with(StatusPreset::Full), &busy_full(), 200));
    for expected in [
        "12s",
        "mlx/qwen3-9b",
        "subagent",
        "projects/dial",
        "main +1 ~2 ?3",
        "ask",
        "workspace",
        "1.2k",
        "27.5 tok/s",
        "v3 · learning idle",
        "3.9k/32k",
    ] {
        assert!(line.contains(expected), "{expected:?} missing: {line:?}");
    }
    let ascii = plain(&border_line(
        &cfg_with(StatusPreset::Full),
        &busy_full(),
        &Theme::dark().with_glyphs(Glyphs::Ascii),
        200,
        Style::default(),
        "+",
        "+",
    ));
    assert!(
        ascii.contains("approval ask") && ascii.contains("sandbox workspace"),
        "without icons the segments carry a word label: {ascii:?}"
    );
    let main_role = StatusData {
        role: "main".into(),
        ..busy_full()
    };
    let line = plain(&render(&cfg_with(StatusPreset::Full), &main_role, 200));
    assert!(
        !line.contains("› main ›"),
        "role segment is hidden for the main role: {line:?}"
    );
}

#[test]
fn custom_lists_use_given_order_and_skip_unknown_names() {
    let cfg = UiConfig {
        status: StatusPreset::Custom,
        status_left: vec![
            "bogus".into(),
            "git".into(),
            " Model ".into(),
            "nope".into(),
        ],
        status_right: vec!["???".into(), "session".into(), "time".into()],
        ..UiConfig::default()
    };
    let line = plain(&render(&cfg, &busy_full(), 120));
    let at = |n: &str| {
        line.find(n)
            .unwrap_or_else(|| panic!("{n:?} missing: {line:?}"))
    };
    assert!(
        at("main") < at("mlx/qwen3-9b")
            && at("mlx/qwen3-9b") < at("abcdef12")
            && at("abcdef12") < at("10:54"),
        "{line:?}"
    );
    assert!(
        !line.contains("bogus")
            && !line.contains("nope")
            && !line.contains("???")
            && !line.contains("abcdef123"),
        "{line:?}"
    );
    let empty = UiConfig {
        status: StatusPreset::Custom,
        status_left: vec![],
        status_right: vec!["bogus".into()],
        ..UiConfig::default()
    };
    assert_eq!(plain(&render(&empty, &data(), 10)), "╭────────╮");
}

#[test]
fn separators_come_from_the_configured_style_and_the_active_symbol_set() {
    let cfg = |separator| UiConfig {
        status_separator: separator,
        status_left: vec![],
        ..UiConfig::default()
    };
    let line_for = |separator, theme: &Theme| {
        let cfg = UiConfig {
            status: StatusPreset::Custom,
            status_left: vec!["model".into(), "path".into()],
            status_right: vec![],
            ..cfg(separator)
        };
        plain(&border_line(
            &cfg,
            &data(),
            theme,
            100,
            Style::default(),
            "╭",
            "╮",
        ))
    };
    let dark = Theme::dark();
    assert!(
        line_for(StatusSeparator::PowerlineThin, &dark).contains("qwen3-9b · "),
        "unicode thin"
    );
    assert!(line_for(StatusSeparator::Powerline, &dark).contains("qwen3-9b · "));
    assert!(line_for(StatusSeparator::Slash, &dark).contains("qwen3-9b / "));
    assert!(line_for(StatusSeparator::Pipe, &dark).contains("qwen3-9b | "));
    assert!(line_for(StatusSeparator::Block, &dark).contains("qwen3-9b █ "));
    let none = line_for(StatusSeparator::None, &dark);
    assert!(
        none.contains("qwen3-9b  ") && !none.contains("qwen3-9b ›") && !none.contains("qwen3-9b /"),
        "{none:?}"
    );
    assert!(
        line_for(StatusSeparator::Ascii, &dark).contains("qwen3-9b > "),
        "ascii separator stays ascii on a unicode theme"
    );
    let nerd = dark.clone().with_glyphs(Glyphs::Nerd);
    assert!(line_for(StatusSeparator::PowerlineThin, &nerd).contains("qwen3-9b · "));
    assert!(line_for(StatusSeparator::Powerline, &nerd).contains("qwen3-9b · "));
}

#[test]
fn context_turns_amber_above_seventy_percent() {
    let theme = Theme::dark();
    let color_of = |used: u64, label: &str| {
        let d = StatusData {
            prompt_tokens: Some(used),
            context_window: 100,
            ..data()
        };
        let line = border_line(
            &UiConfig::default(),
            &d,
            &theme,
            120,
            Style::default(),
            "╭",
            "╮",
        );
        line.spans
            .iter()
            .find(|s| s.content.trim() == label)
            .unwrap_or_else(|| panic!("{label} not found in {:?}", plain(&line)))
            .style
            .fg
    };
    assert_eq!(color_of(59, "━━━━"), theme.fg(Tok::Muted).fg);
    assert_eq!(color_of(70, "━━━━"), theme.fg(Tok::Muted).fg);
    assert_eq!(color_of(71, "71%"), theme.fg(Tok::Accent).fg);
    assert_eq!(color_of(85, "85%"), theme.fg(Tok::Accent).fg);
    assert_ne!(theme.fg(Tok::Muted).fg, theme.fg(Tok::Accent).fg);
}

#[test]
fn context_estimates_update_percentage_and_count_and_are_marked() {
    let mut d = data();
    d.context_estimated = true;
    let before = plain(&render(&UiConfig::default(), &d, 120));
    assert!(
        !before.contains("12%") && before.contains("~3.9k/32k"),
        "{before}"
    );
    d.prompt_tokens = Some(16_000);
    let after = plain(&render(&UiConfig::default(), &d, 120));
    assert!(
        !after.contains("50%") && after.contains("~16k/32k"),
        "{after}"
    );
    d.context_estimated = false;
    let exact = plain(&render(&UiConfig::default(), &d, 120));
    assert!(!exact.contains("50%") && exact.contains("16k/32k"));
    assert!(!exact.contains("~50%") && !exact.contains("~16k/32k"));
}

#[test]
fn context_ruler_is_six_cells_and_human_units_are_floored() {
    let line_at = |used: u64, window: u32| {
        let d = StatusData {
            prompt_tokens: Some(used),
            context_window: window,
            ..data()
        };
        plain(&render(&UiConfig::default(), &d, 120))
    };
    assert!(
        line_at(50, 100).contains("━━━─── 50/100"),
        "{}",
        line_at(50, 100)
    );
    assert!(
        line_at(4, 100).contains("━───── 4/100"),
        "any usage shows at least one cell"
    );
    assert!(line_at(100, 100).contains("100% ━━━━━━ 100/100"));
    assert!(
        line_at(0, 100).contains("────── 0/100"),
        "a known zero is shown as zero"
    );
    assert!(line_at(3900, 32_768).contains("3.9k/32k"));
    assert!(line_at(4000, 131_072).contains("4k/131k"));
    assert!(line_at(1_250_000, 2_000_000).contains("1.2M/2M"));
    assert_eq!(human(999), "999");
    assert_eq!(elapsed(5.9), "5s");
    assert_eq!(elapsed(65.0), "1m05s");
    assert_eq!(elapsed(3720.0), "1h02m");
}

#[test]
fn the_status_line_paints_no_background_with_or_without_transparency() {
    let theme = Theme::dark();
    for transparent in [false, true] {
        let line = border_line(
            &UiConfig {
                status_transparent: transparent,
                ..UiConfig::default()
            },
            &data(),
            &theme,
            100,
            Style::default(),
            "╭",
            "╮",
        );
        assert!(
            line.spans.iter().all(|s| s.style.bg.is_none()),
            "{:?}",
            line.spans
        );
    }
}

#[test]
fn spinner_is_busy_only_and_animates_with_the_tick() {
    let cfg = UiConfig::default();
    let theme = Theme::dark();
    let at = |tick: u64, phase: Phase| {
        plain(&border_line(
            &cfg,
            &StatusData {
                phase,
                tick,
                elapsed_secs: 12.0,
                ..data()
            },
            &theme,
            100,
            Style::default(),
            "╭",
            "╮",
        ))
    };
    let (a, b) = (at(0, Phase::Working), at(1, Phase::Working));
    assert!(
        a.contains(theme.spinner(0))
            && b.contains(theme.spinner(1))
            && a.contains("12s")
            && b.contains("12s"),
        "{a:?} {b:?}"
    );
    assert_ne!(a, b);
    let idle = at(0, Phase::Idle);
    assert!(
        !idle.contains("12s") && !idle.contains(theme.spinner(0)),
        "{idle:?}"
    );
    let ascii = border_line(
        &cfg,
        &StatusData {
            phase: Phase::Working,
            tick: 1,
            ..data()
        },
        &theme.clone().with_glyphs(Glyphs::Ascii),
        100,
        Style::default(),
        "+",
        "+",
    );
    assert!(plain(&ascii).is_ascii());
}

#[test]
fn untrusted_status_text_cannot_inject_terminal_sequences() {
    let hostile = StatusData {
        model: "evil\x1b]0;owned\x07model".into(),
        branch: "br\x1b[31manch\n2".into(),
        ..data()
    };
    let line = plain(&render(&UiConfig::default(), &hostile, 120));
    assert!(
        !line.contains('\x1b') && !line.contains('\x07') && !line.contains('\n'),
        "{line:?}"
    );
    assert!(
        line.contains("evilmodel") && line.contains("branch 2"),
        "{line:?}"
    );
}

#[test]
fn wide_and_emoji_text_is_measured_in_cells() {
    let wide = StatusData {
        model: "日本語モデル🦀-large".into(),
        project_path: "/w/日本語/プロジェクト".into(),
        branch: "機能/🦀".into(),
        ..data()
    };
    for width in 0..=90u16 {
        let line = render(&UiConfig::default(), &wide, width);
        assert_eq!(
            text::line_width(&line),
            width as usize,
            "width {width}: {:?}",
            plain(&line)
        );
    }
}

#[test]
fn paths_and_model_ids_are_shortened_to_what_the_user_recognizes() {
    assert_eq!(tilde("/Users/me/dev/dial", Some("/Users/me")), "~/dev/dial");
    assert_eq!(tilde("/Users/me", Some("/Users/me/")), "~");
    assert_eq!(
        tilde("/Users/mel/x", Some("/Users/me")),
        "/Users/mel/x",
        "a sibling directory with the same prefix is not home"
    );
    assert_eq!(tilde("/tmp/x", None), "/tmp/x");
    assert_eq!(last_components("~/dev/projects/dial", 2), "projects/dial");
    assert_eq!(last_components("~", 2), "~");
    assert_eq!(last_components("/tmp/x", 2), "/tmp/x");
    assert_eq!(last_components("/", 2), "/");
    assert_eq!(
        short_model("/cache/hub/models--mlx-community--Qwen3-9B/snapshots/abc"),
        "mlx-community/Qwen3-9B"
    );
    assert_eq!(short_model("/models/local/qwen.gguf"), "qwen.gguf");
    assert_eq!(short_model("qwen3:9b"), "qwen3:9b");
}

#[test]
fn preview_row_has_no_corners_and_the_corner_variant_does() {
    let preview = plain(&preview_row(
        &UiConfig::default(),
        &data(),
        &Theme::dark(),
        90,
    ));
    assert!(
        !preview.contains('╭') && !preview.contains('╮') && preview.contains("mlx/qwen3-9b"),
        "{preview:?}"
    );
    let boxed = plain(&render(&UiConfig::default(), &data(), 90));
    assert!(boxed.starts_with('╭') && boxed.ends_with('╮'));
    let ascii = border_line(
        &UiConfig::default(),
        &data(),
        &Theme::dark().with_glyphs(Glyphs::Ascii),
        90,
        Style::default(),
        "+",
        "+",
    );
    let ascii = plain(&ascii);
    assert!(
        ascii.starts_with("+-") && ascii.ends_with("-+") && ascii.is_ascii(),
        "{ascii:?}"
    );
}

#[test]
fn border_style_applies_to_the_rule_only() {
    let border = Style::default().fg(Color::Magenta);
    let line = border_line(
        &UiConfig::default(),
        &data(),
        &Theme::dark(),
        100,
        border,
        "╭",
        "╮",
    );
    assert_eq!(line.spans.first().unwrap().style, border);
    assert_eq!(line.spans.last().unwrap().style, border);
    let model = line
        .spans
        .iter()
        .find(|s| s.content.contains("qwen3-9b"))
        .unwrap();
    assert_ne!(model.style.fg, Some(Color::Magenta));
}

#[test]
fn usage_segment_is_outside_full_and_renders_when_configured() {
    let with = StatusData {
        usage: Some("anthropic 5h 80% · resets 2h".into()),
        ..data()
    };
    // The Full preset has no usage segment.
    assert!(!plain(&render(&cfg_with(StatusPreset::Full), &with, 160)).contains("anthropic"));
    let custom = UiConfig {
        status: StatusPreset::Custom,
        status_left: Vec::new(),
        status_right: vec!["usage".to_owned()],
        ..UiConfig::default()
    };
    assert!(plain(&render(&custom, &with, 160)).contains("anthropic 5h 80%"));
    let without = StatusData {
        usage: None,
        ..data()
    };
    assert!(!plain(&render(&custom, &without, 160)).contains("anthropic"));
}

#[test]
fn workers_segment_shows_running_out_of_total_and_hides_when_idle() {
    let busy = StatusData {
        workers: Some(crate::ui::worker_pins::WorkerTally {
            running: 2,
            total: 3,
            cost_usd: None,
        }),
        ..data()
    };
    assert!(plain(&render(&cfg_with(StatusPreset::Default), &busy, 160)).contains("2/3 workers"));
    let priced = StatusData {
        workers: Some(crate::ui::worker_pins::WorkerTally {
            running: 2,
            total: 3,
            cost_usd: Some(0.14),
        }),
        ..data()
    };
    assert!(
        plain(&render(&cfg_with(StatusPreset::Default), &priced, 160))
            .contains("2/3 workers · $0.14")
    );
    let idle = StatusData {
        workers: Some(crate::ui::worker_pins::WorkerTally {
            running: 0,
            total: 3,
            cost_usd: None,
        }),
        ..data()
    };
    assert!(!plain(&render(&cfg_with(StatusPreset::Default), &idle, 160)).contains("workers"));
}

#[test]
fn narrow_lines_drop_whole_segments_instead_of_leaving_stubs() {
    let cfg = UiConfig::default();
    let data = StatusData {
        provider: "local".into(),
        model: "qwen3.5-9b".into(),
        project_path: "/work/projects/atlas".into(),
        branch: "main".into(),
        context_window: 32_768,
        ..StatusData::default()
    };
    for width in 20..=80u16 {
        let line = plain(&render(&cfg, &data, width));
        // A path is either recognisable (its last component whole) or absent.
        if line.contains('…') && !line.contains("qwen") {
            continue;
        }
        let path_stub = line
            .split(['╭', '─', '·', ' '])
            .any(|part| part.starts_with('…') && !part.ends_with("atlas"));
        assert!(!path_stub, "{width}: {line:?}");
        if line.contains("atlas") {
            assert!(
                line.contains("qwen3.5-9b"),
                "path outlived the model: {line:?}"
            );
        }
    }
    // Before the model name is cut, its provider prefix goes.
    let line = plain(&render(&cfg, &data, 24));
    assert!(
        line.contains("qwen3.5-9b") && !line.contains("local/"),
        "{line:?}"
    );
}
