use super::*;

fn env(pairs: &[(&str, &str)]) -> EnvFn {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Box::new(move |k| map.get(k).cloned())
}
fn rgb_env() -> EnvFn {
    env(&[("COLORTERM", "truecolor")])
}
fn write_theme(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir.join("themes")).unwrap();
    fs::write(dir.join("themes").join(format!("{name}.json")), body).unwrap();
}
fn manager(dir: &Path, cfg: &UiConfig) -> ThemeManager {
    ThemeManager::with_env(dir, cfg, Some(true), rgb_env())
}

#[test]
fn every_token_and_symbol_has_a_unique_id_that_round_trips() {
    let mut seen = std::collections::HashSet::new();
    for tok in Tok::ALL {
        assert!(seen.insert(tok.id()), "duplicate id {}", tok.id());
        assert_eq!(Tok::from_id(tok.id()), Some(*tok));
        assert_eq!(
            Tok::ALL[tok.index()],
            *tok,
            "ALL must be in declaration order"
        );
    }
    let ids: std::collections::HashSet<String> = Sym::ALL.iter().map(|s| s.id()).collect();
    assert_eq!(ids.len(), Sym::ALL.len());
    assert!(Sym::ALL.iter().all(|s| Sym::from_id(&s.id()) == Some(*s)));
    assert_eq!(Sym::BoxTl.id(), "boxTl");
}

#[test]
fn embedded_themes_are_valid_complete_and_distinct() {
    for (name, _) in BUILTINS {
        let theme = Theme::builtin(name).unwrap_or_else(|| panic!("{name} must load"));
        assert_eq!(theme.name(), *name);
        assert_eq!(theme.styles.len(), Tok::ALL.len());
    }
    for (i, (a, _)) in BUILTINS.iter().enumerate() {
        for (b, _) in &BUILTINS[i + 1..] {
            assert_ne!(
                Theme::builtin(a).unwrap().styles,
                Theme::builtin(b).unwrap().styles,
                "{a} and {b} must not render identically"
            );
        }
    }
    // Root themes must define every token themselves (checked by the parser); extending ones override some.
    assert!(Theme::dark().fg(Tok::Accent) != Theme::builtin("gruvbox").unwrap().fg(Tok::Accent));
    assert!(
        Theme::dark().is_dark()
            && !Theme::light().is_dark()
            && !Theme::builtin("paper").unwrap().is_dark()
    );
    assert!(
        Theme::mono()
            .fg(Tok::Accent)
            .add_modifier
            .contains(Modifier::BOLD),
        "mono conveys structure with modifiers"
    );
    assert!(
        Theme::mono()
            .bg(Tok::SelectedBg)
            .add_modifier
            .contains(Modifier::REVERSED)
    );
    assert_eq!(
        Theme::dark().bg(Tok::UserBg).bg,
        Some(Color::Rgb(0x1d, 0x1b, 0x18))
    );
}

#[test]
fn ported_themes_define_every_syntax_token_explicitly() {
    const SYNTAX: &[&str] = &[
        "syntax.keyword",
        "syntax.string",
        "syntax.number",
        "syntax.comment",
        "syntax.type",
        "syntax.function",
        "syntax.variable",
        "syntax.operator",
        "syntax.punctuation",
    ];
    for (name, text) in BUILTINS {
        if BUILTIN_ROOTS.contains(name) || matches!(*name, "gruvbox" | "nord" | "paper") {
            continue;
        }
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let colors = json["colors"].as_object().expect("colors object");
        for id in SYNTAX {
            assert!(colors.contains_key(*id), "{name} must set {id}");
        }
    }
}

#[test]
fn background_and_text_tokens_stay_separate() {
    let theme = Theme::dark();
    assert!(theme.bg(Tok::UserBg).bg.is_some() && theme.fg(Tok::UserBg).bg.is_none());
    assert!(theme.fg(Tok::Muted).fg.is_some() && theme.bg(Tok::Muted).fg.is_none());
    let combined = theme.on(Tok::UserText, Tok::UserBg);
    assert!(combined.fg.is_some() && combined.bg.is_some());
}

#[test]
fn custom_theme_resolves_vars_extends_numbers_defaults_and_modifiers() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(
        dir.path(),
        "mine",
        r##"{
            "extends": "light", "dark": false,
            "vars": { "brand": "#336699", "alias": "brand", "gray": 244 },
            "colors": { "accent": "alias", "muted": "gray", "text": "", "toolTitle": { "fg": "brand", "bold": true, "italic": true },
                        "userBg": "#abc", "error": "bright-red" }
        }"##,
    );
    let cfg = UiConfig {
        theme: "mine".into(),
        ..UiConfig::default()
    };
    let m = manager(dir.path(), &cfg);
    assert!(m.last_error().is_none(), "{:?}", m.last_error());
    let t = m.current();
    assert_eq!(
        t.fg(Tok::Accent).fg,
        Some(Color::Rgb(0x33, 0x66, 0x99)),
        "variables resolve through aliases"
    );
    assert_eq!(t.fg(Tok::Muted).fg, Some(Color::Indexed(244)));
    assert_eq!(
        t.fg(Tok::Text).fg,
        Some(Color::Reset),
        "empty string means the terminal default"
    );
    assert!(
        t.fg(Tok::ToolTitle)
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC)
    );
    assert_eq!(
        t.bg(Tok::UserBg).bg,
        Some(Color::Rgb(0xaa, 0xbb, 0xcc)),
        "#rgb expands"
    );
    assert_eq!(t.fg(Tok::Error).fg, Some(Color::LightRed));
    assert_eq!(
        t.fg(Tok::Warning),
        Theme::light().fg(Tok::Warning),
        "unmentioned tokens inherit from extends"
    );
    assert!(!t.is_dark());
}

#[test]
fn invalid_themes_are_rejected_with_actionable_messages() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "badtoken",
            r##"{"colors":{"accnt":"#fff"}}"##,
            "unknown color token(s): accnt",
        ),
        ("badkey", r#"{"colours":{}}"#, "unknown top-level key"),
        (
            "badhex",
            r##"{"colors":{"accent":"#12"}}"##,
            "invalid hex color",
        ),
        (
            "missingvar",
            r#"{"colors":{"accent":"nope"}}"#,
            "neither a color nor a defined variable",
        ),
        (
            "cycle",
            r#"{"vars":{"a":"b","b":"a"},"colors":{"accent":"a"}}"#,
            "variable cycle",
        ),
        ("range", r#"{"colors":{"accent":300}}"#, "outside 0-255"),
        (
            "style",
            r##"{"colors":{"accent":{"fg":"#fff","glow":true}}}"##,
            "unknown style key",
        ),
        ("notjson", "{", "invalid JSON"),
        ("selfext", r#"{"extends":"selfext"}"#, "cyclic"),
        ("noparent", r#"{"extends":"ghost"}"#, "unknown theme"),
        (
            "symbol",
            r#"{"symbols":{"overrides":{"nosuch":"x"}}}"#,
            "unknown symbol",
        ),
        (
            "ctrl",
            "{\"symbols\":{\"overrides\":{\"bullet\":\"\\u001b[31m\"}}}",
            "control characters",
        ),
    ];
    for (name, body, _) in cases {
        write_theme(dir.path(), name, body);
    }
    let m = manager(dir.path(), &UiConfig::default());
    let entries = m.names();
    for (name, _, expect) in cases {
        let entry = entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} not listed"));
        let error = entry
            .error
            .as_deref()
            .unwrap_or_else(|| panic!("{name} should have failed"));
        assert!(error.contains(expect), "{name}: {error}");
    }
}

#[test]
fn unknown_or_hostile_names_fall_back_to_dark_and_report() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["nope", "../../etc/passwd", "a/b", ""] {
        let cfg = UiConfig {
            theme: bad.into(),
            ..UiConfig::default()
        };
        let mut m = manager(dir.path(), &UiConfig::default());
        assert!(m.apply_config(&cfg, Some(true)).is_err(), "{bad:?}");
        assert_eq!(m.current().name(), "dark");
        assert!(m.last_error().is_some());
    }
}

#[test]
fn colors_downgrade_to_the_terminal_and_no_color_wins() {
    assert_eq!(
        detect_color_mode(&*env(&[("COLORTERM", "24bit")])),
        ColorMode::TrueColor
    );
    assert_eq!(
        detect_color_mode(&*env(&[("TERM", "xterm-256color")])),
        ColorMode::Ansi256
    );
    assert_eq!(
        detect_color_mode(&*env(&[("TERM", "xterm")])),
        ColorMode::Ansi16
    );
    assert_eq!(
        detect_color_mode(&*env(&[("TERM", "dumb")])),
        ColorMode::None
    );
    assert_eq!(
        detect_color_mode(&*env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
        ColorMode::None
    );
    assert_eq!(
        detect_color_mode(&*env(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])),
        ColorMode::TrueColor,
        "empty NO_COLOR is ignored"
    );

    let dir = tempfile::tempdir().unwrap();
    let cfg = UiConfig::default();
    let m256 = ThemeManager::with_env(
        dir.path(),
        &cfg,
        Some(true),
        env(&[("TERM", "xterm-256color")]),
    );
    assert!(
        matches!(m256.current().fg(Tok::Accent).fg, Some(Color::Indexed(_))),
        "rgb became an index"
    );
    assert_eq!(rgb_to_256((255, 255, 255)), 231);
    assert_eq!(rgb_to_256((0, 0, 0)), 16);
    assert!(
        (232..=255).contains(&rgb_to_256((128, 128, 128))),
        "grays use the gray ramp"
    );
    let m16 = ThemeManager::with_env(dir.path(), &cfg, Some(true), env(&[("TERM", "xterm")]));
    assert!(
        !matches!(
            m16.current().fg(Tok::Accent).fg,
            Some(Color::Rgb(..) | Color::Indexed(_))
        ),
        "16-color has only named colors"
    );

    let none = ThemeManager::with_env(
        dir.path(),
        &UiConfig {
            theme: "gruvbox".into(),
            ..cfg
        },
        Some(true),
        env(&[("NO_COLOR", "1")]),
    );
    assert_eq!(
        none.current().name(),
        "mono",
        "NO_COLOR overrides the configured theme"
    );
    for tok in Tok::ALL {
        let (f, b) = (none.current().fg(*tok), none.current().bg(*tok));
        assert!(
            matches!(f.fg, None | Some(Color::Reset)) && matches!(b.bg, None | Some(Color::Reset)),
            "{tok:?} must carry no color"
        );
    }
}

#[test]
fn symbol_presets_overrides_and_spinner() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(
        dir.path(),
        "glyphs",
        r#"{"symbols":{"preset":"nerd","overrides":{"bullet":"•"},"spinner":["a","b"]}}"#,
    );
    let mut cfg = UiConfig {
        theme: "glyphs".into(),
        ..UiConfig::default()
    };
    let mut m = manager(dir.path(), &cfg);
    assert_eq!(
        m.current().glyphs(),
        Glyphs::Nerd,
        "auto follows the theme's preferred preset"
    );
    assert_eq!(m.current().sym(Sym::Bullet), "•");
    assert_eq!((m.current().spinner(0), m.current().spinner(3)), ("a", "b"));
    cfg.symbols = SymbolPreset::Ascii;
    m.apply_config(&cfg, Some(true)).unwrap();
    assert_eq!(
        m.current().glyphs(),
        Glyphs::Ascii,
        "an explicit preset beats the theme's preference"
    );
    assert!(m.current().sym(Sym::BoxTl).is_ascii() && m.current().sym(Sym::Prompt).is_ascii());
    assert_eq!(
        m.current().sym(Sym::Bullet),
        "•",
        "overrides are the user's explicit choice in every preset"
    );
    // Auto without a theme preference: unicode unless DIAL_NERD_FONT opts in.
    let plain = UiConfig::default();
    assert_eq!(
        manager(dir.path(), &plain).current().glyphs(),
        Glyphs::Unicode
    );
    let nerd = ThemeManager::with_env(
        dir.path(),
        &plain,
        Some(true),
        env(&[("DIAL_NERD_FONT", "1")]),
    );
    assert_eq!(nerd.current().glyphs(), Glyphs::Nerd);
    for sym in Sym::ALL {
        assert!(
            Theme::dark()
                .with_glyphs(Glyphs::Ascii)
                .sym(*sym)
                .is_ascii(),
            "{sym:?} must have an ascii form"
        );
    }
}

#[test]
fn auto_theme_picks_the_slot_for_the_terminal_background() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = UiConfig {
        theme: "auto".into(),
        theme_dark: "nord".into(),
        theme_light: "paper".into(),
        ..UiConfig::default()
    };
    assert_eq!(
        ThemeManager::with_env(dir.path(), &cfg, Some(true), rgb_env())
            .current()
            .name(),
        "nord"
    );
    assert_eq!(
        ThemeManager::with_env(dir.path(), &cfg, Some(false), rgb_env())
            .current()
            .name(),
        "paper"
    );
    assert_eq!(
        ThemeManager::with_env(dir.path(), &cfg, None, rgb_env())
            .current()
            .name(),
        "nord",
        "unknown background means dark"
    );
}

#[test]
fn preview_and_restore_do_not_touch_the_configured_theme() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = manager(dir.path(), &UiConfig::default());
    m.preview("gruvbox").unwrap();
    assert_eq!(m.current().name(), "gruvbox");
    assert!(m.preview("ghost").is_err());
    assert_eq!(
        m.current().name(),
        "gruvbox",
        "a failed preview keeps the previous preview"
    );
    m.restore();
    assert_eq!(m.current().name(), "dial-dark");
}

#[test]
fn custom_themes_hot_reload_and_a_broken_edit_keeps_the_last_good_theme() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(dir.path(), "live", r##"{"colors":{"accent":"#111111"}}"##);
    let cfg = UiConfig {
        theme: "live".into(),
        ..UiConfig::default()
    };
    let mut m = manager(dir.path(), &cfg);
    assert_eq!(
        m.current().fg(Tok::Accent).fg,
        Some(Color::Rgb(0x11, 0x11, 0x11))
    );
    assert!(!m.reload_if_changed(), "unchanged file is not reloaded");
    let path = dir.path().join("themes/live.json");
    fs::write(&path, r##"{"colors":{"accent":"#222222"}, "dark": true}"##).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    assert!(m.reload_if_changed());
    assert_eq!(
        m.current().fg(Tok::Accent).fg,
        Some(Color::Rgb(0x22, 0x22, 0x22))
    );
    fs::write(&path, "{ not json").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
        .unwrap();
    assert!(!m.reload_if_changed());
    assert_eq!(
        m.current().fg(Tok::Accent).fg,
        Some(Color::Rgb(0x22, 0x22, 0x22)),
        "last good theme stays active"
    );
    assert!(m.last_error().is_some_and(|e| e.contains("invalid JSON")));
    assert!(
        !m.reload_if_changed(),
        "the same broken file is not retried every poll"
    );
}

#[test]
fn built_in_names_win_and_listing_includes_custom_themes() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(dir.path(), "dark", r##"{"colors":{"accent":"#ff0000"}}"##);
    write_theme(dir.path(), "zebra", "{}");
    write_theme(dir.path(), "alpha", "{}");
    let m = manager(dir.path(), &UiConfig::default());
    assert_eq!(
        m.current().fg(Tok::Accent),
        Theme::dark().fg(Tok::Accent),
        "a custom file cannot shadow a built-in"
    );
    let names: Vec<(String, bool)> = m.names().into_iter().map(|e| (e.name, e.builtin)).collect();
    let custom: Vec<&str> = names
        .iter()
        .filter(|(_, b)| !*b)
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(custom, ["alpha", "zebra"]);
    assert!(
        names.iter().take(BUILTINS.len()).all(|(_, b)| *b),
        "built-ins first"
    );
}

#[test]
fn exported_themes_round_trip_and_color_blind_mode_only_changes_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), &UiConfig::default());
    for name in ["dark", "mono", "gruvbox"] {
        let json = m.export_json(name).unwrap();
        let reloaded = parse_theme(
            &json,
            name,
            &|b| Theme::builtin(b).ok_or_else(|| "no".into()),
            true,
        )
        .unwrap();
        let original = Theme::builtin(name).unwrap();
        assert_eq!(
            reloaded.styles, original.styles,
            "{name} must survive export -> load"
        );
    }
    let normal = Theme::dark();
    let blind = Theme::dark().color_blind();
    for tok in Tok::ALL {
        let changed = normal.fg(*tok) != blind.fg(*tok);
        assert_eq!(
            changed,
            matches!(tok, Tok::DiffAdded | Tok::DiffRemoved),
            "{tok:?}"
        );
    }
    assert_ne!(
        blind.fg(Tok::DiffAdded).fg,
        Some(Color::Rgb(0x9e, 0xce, 0x6a))
    );
}

#[test]
fn built_in_themes_use_their_declared_rgb_colors_not_the_terminals_named_ones() {
    // The shipped themes name variables `blue`, `red`, `gray`…; they must resolve to the RGB values
    // declared in the file, never to the terminal palette entries of the same name.
    let dark = Theme::dark();
    assert_eq!(dark.fg(Tok::Accent).fg, Some(Color::Rgb(0xf5, 0xb1, 0x4c)));
    assert_eq!(
        dark.fg(Tok::DiffAdded).fg,
        Some(Color::Rgb(0x8f, 0xd6, 0x94))
    );
    assert_eq!(dark.fg(Tok::Error).fg, Some(Color::Rgb(0xff, 0x6b, 0x81)));
    assert_eq!(dark.fg(Tok::Muted).fg, Some(Color::Rgb(0x7c, 0x82, 0x93)));
    for name in ["dark", "light", "gruvbox", "nord", "paper"] {
        let theme = Theme::builtin(name).unwrap();
        let named = Tok::ALL
            .iter()
            .filter(|t| {
                matches!(
                    theme.fg(**t).fg,
                    Some(
                        Color::Red
                            | Color::Blue
                            | Color::Green
                            | Color::Yellow
                            | Color::Cyan
                            | Color::Gray
                    )
                )
            })
            .count();
        assert_eq!(
            named, 0,
            "{name} must not fall back to terminal-named colors"
        );
    }
}

#[test]
fn a_variable_shadows_a_named_color_but_a_self_reference_means_the_terminal_color() {
    let vars: Map<String, Value> =
        serde_json::from_str(r##"{"red":"#010203","blue":"blue","loop":"loop"}"##).unwrap();
    let resolve = |s: &str| resolve_color(&Value::String(s.into()), &vars, &mut Vec::new());
    assert_eq!(resolve("red"), Ok(Color::Rgb(1, 2, 3)));
    assert_eq!(
        resolve("blue"),
        Ok(Color::Blue),
        "`blue` -> `blue` is the ANSI color"
    );
    assert_eq!(
        resolve("green"),
        Ok(Color::Green),
        "unshadowed names still work"
    );
    assert!(resolve("loop").unwrap_err().contains("cycle"));
}
