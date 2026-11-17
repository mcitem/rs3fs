use crate::services::settings::{ConnectionSettings, write_settings};
use gpui_kit::{
    App, Global, Window,
    component::{ActiveTheme, Theme, ThemeMode, highlighter::HighlightThemeStyle},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Global for Appearance {}

pub(crate) fn init(cx: &mut App) {
    let preference = settings_path()
        .and_then(|path| Ok(serde_json::from_slice(&std::fs::read(path)?)?))
        .unwrap_or_default();
    cx.set_global::<Appearance>(preference);
}

pub(crate) fn attach(window: &mut Window, cx: &mut App) {
    apply(window, cx);
    window
        .observe_window_appearance(|window, cx| {
            if *cx.global::<Appearance>() == Appearance::System {
                apply(window, cx);
            }
        })
        .detach();
}

pub(crate) fn set(preference: Appearance, window: &mut Window, cx: &mut App) -> anyhow::Result<()> {
    write_settings(&settings_path()?, &preference)?;
    cx.set_global(preference);
    apply(window, cx);
    Ok(())
}

fn apply(window: &mut Window, cx: &mut App) {
    let mode = match cx.global::<Appearance>() {
        Appearance::System => ThemeMode::from(window.appearance()),
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
    };
    Theme::change(mode, Some(window), cx);
    let mut highlight = (*cx.theme().highlight_theme).clone();
    highlight.name = if mode.is_dark() {
        "Dark Modern"
    } else {
        "Light Modern"
    }
    .into();
    let style = modern_highlight(mode.is_dark());
    highlight.style.editor_background = style.editor_background;
    highlight.style.editor_foreground = style.editor_foreground;
    highlight.style.editor_line_number = style.editor_line_number;
    highlight.style.editor_active_line_number = style.editor_active_line_number;
    highlight.style.editor_gutter_background = style.editor_background;
    highlight.style.syntax = style.syntax;
    Theme::update(cx, |theme| theme.highlight_theme = Arc::new(highlight));
    crate::ui::zoom::apply(cx);
}

fn modern_highlight(dark: bool) -> HighlightThemeStyle {
    // Modern inherits the VS Code Dark+/Light+ token colors. Map those
    // colors to Tree-sitter captures, which differ from TextMate scopes.
    let mut syntax = serde_json::Map::new();
    for (captures, light, dark_color) in [
        ("comment comment.doc", "#008000", "#6A9955"),
        ("keyword boolean preproc", "#0000FF", "#569CD6"),
        ("string string.special text.code.span", "#A31515", "#CE9178"),
        ("string.escape string.regex", "#811F3F", "#D7BA7D"),
        ("number", "#098658", "#B5CEA8"),
        ("function", "#795E26", "#DCDCAA"),
        ("type constructor enum", "#267F99", "#4EC9B0"),
        ("variable property variable.special", "#001080", "#9CDCFE"),
        ("constant", "#0070C1", "#4FC1FF"),
        ("tag tag.doctype", "#800000", "#569CD6"),
        ("attribute", "#E50000", "#9CDCFE"),
        ("operator punctuation", "#000000", "#D4D4D4"),
        ("link_text link_uri title", "#0000FF", "#569CD6"),
    ] {
        for capture in captures.split_whitespace() {
            syntax.insert(
                capture.into(),
                serde_json::json!({"color": if dark { dark_color } else { light }}),
            );
        }
    }
    serde_json::from_value(serde_json::json!({
        "editor.background": if dark { "#1F1F1F" } else { "#FFFFFF" },
        "editor.foreground": if dark { "#CCCCCC" } else { "#3B3B3B" },
        "editor.line_number": "#6E7681",
        "editor.active_line_number": if dark { "#CCCCCC" } else { "#171184" },
        "syntax": syntax,
    }))
    .expect("valid Modern highlight palette")
}

fn settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(ConnectionSettings::path()?.with_file_name("appearance.json"))
}
