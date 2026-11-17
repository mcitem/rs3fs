use crate::services::settings::{ConnectionSettings, write_settings};
use gpui_kit::{
    component::{ActiveTheme, Theme},
    *,
};

actions!(ui_zoom, [ZoomIn, ZoomOut, ResetZoom]);

const MIN_LEVEL: i8 = -3;
const MAX_LEVEL: i8 = 2;

struct Zoom {
    level: i8,
    base_font: Pixels,
    base_mono_font: Pixels,
}

impl Global for Zoom {}

pub(crate) fn init(cx: &mut App) {
    let level = settings_path()
        .and_then(|path| Ok(serde_json::from_slice::<i8>(&std::fs::read(path)?)?))
        .unwrap_or_default()
        .clamp(MIN_LEVEL, MAX_LEVEL);
    cx.set_global(Zoom {
        level,
        base_font: cx.theme().font_size,
        base_mono_font: cx.theme().mono_font_size,
    });
    apply(cx);
    let bindings = [
        ("secondary-=", ZoomIn.boxed_clone()),
        ("secondary-shift-=", ZoomIn.boxed_clone()),
        ("secondary-+", ZoomIn.boxed_clone()),
        ("secondary--", ZoomOut.boxed_clone()),
        ("secondary-0", ResetZoom.boxed_clone()),
    ]
    .map(|(keys, action)| KeyBinding::load(keys, action, None, false, None, cx).unwrap());
    cx.bind_keys(bindings);
    cx.on_action(|_: &ZoomIn, cx| set_level(cx.global::<Zoom>().level + 1, cx));
    cx.on_action(|_: &ZoomOut, cx| set_level(cx.global::<Zoom>().level - 1, cx));
    cx.on_action(|_: &ResetZoom, cx| set_level(0, cx));
}

fn scale(level: i8) -> f32 {
    1.2_f32.powi(i32::from(level))
}

fn set_level(level: i8, cx: &mut App) {
    let level = level.clamp(MIN_LEVEL, MAX_LEVEL);
    if cx.global::<Zoom>().level == level {
        return;
    }
    cx.global_mut::<Zoom>().level = level;
    apply(cx);
    if let Err(error) = settings_path().and_then(|path| write_settings(&path, &level)) {
        eprintln!("无法保存界面缩放：{error:#}");
    }
}

pub(crate) fn apply(cx: &mut App) {
    let zoom = cx.global::<Zoom>();
    let scale = scale(zoom.level);
    let font = zoom.base_font * scale;
    let mono_font = zoom.base_mono_font * scale;
    Theme::update(cx, |theme| {
        theme.font_size = font;
        theme.mono_font_size = mono_font;
    });
    cx.refresh_windows();
}

fn settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(ConnectionSettings::path()?.with_file_name("zoom.json"))
}
