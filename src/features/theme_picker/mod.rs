//! Theme picker: Zed's theme registry, applied live and written to the
//! user's settings so the next start keeps it.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::AsyncApp;
use theme::{ActiveTheme as _, GlobalTheme, SystemAppearance, ThemeRegistry};
use zed_fs::Fs;

use crate::{
    app::{
        feature::Ctx,
        overlay::{Overlay, PickerOwner, PickerPayload},
    },
    terminal::{
        picker::{PickerEntry, PickerList},
        prompt::LinePrompt,
    },
    zed,
};

const TITLE: &str = "Themes";

#[derive(Default)]
pub struct ThemePicker {
    /// The names behind the open picker's `Index` payloads.
    names: Vec<String>,
}

impl ThemePicker {
    /// Lists every registered theme, the active one first.
    pub fn open(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let (active, mut themes) = cx.update(|cx| {
            let active = cx.theme().name.to_string();
            let themes = ThemeRegistry::global(cx)
                .list()
                .into_iter()
                .map(|meta| (meta.name.to_string(), meta.appearance.is_light()))
                .collect::<Vec<_>>();
            (active, themes)
        });
        themes.sort_by(|left, right| {
            (left.0 != active)
                .cmp(&(right.0 != active))
                .then_with(|| left.0.cmp(&right.0))
        });
        let entries = themes
            .iter()
            .enumerate()
            .map(|(index, (name, light))| PickerEntry {
                label: name.clone(),
                detail: format!("({})", if *light { "light" } else { "dark" }),
                enabled: true,
                payload: PickerPayload::Index(index),
            })
            .collect();
        self.names = themes.into_iter().map(|(name, _)| name).collect();
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Picker {
            title: TITLE,
            query: LinePrompt::new(),
            list: PickerList::new(entries),
            owner: PickerOwner::Theme,
        });
    }

    /// Applies the picked theme to every open editor and saves it.
    pub fn pick(&mut self, ctx: &mut Ctx, index: usize, cx: &mut AsyncApp) {
        ctx.overlays.pop();
        let Some(name) = self.names.get(index).cloned() else {
            return;
        };
        match apply(&name, ctx.services.fs.clone(), cx) {
            Ok(()) => {
                for (_, document) in ctx.documents.iter() {
                    let _ = zed::editor::refresh_style(&document.editor, cx);
                }
                ctx.status.set(format!("theme: {name}"));
            }
            Err(error) => ctx.status.set(format!("theme failed: {error:#}")),
        }
    }
}

fn apply(name: &str, fs: Arc<dyn Fs>, cx: &mut AsyncApp) -> Result<()> {
    cx.update(|cx| {
        let theme = ThemeRegistry::global(cx)
            .get(name)
            .with_context(|| format!("load theme {name}"))?;
        let theme_name: Arc<str> = theme.name.as_str().into();
        let appearance = theme.appearance();
        let system_appearance = SystemAppearance::global(cx).0;
        GlobalTheme::update_theme(cx, theme);
        settings::update_settings_file(fs, cx, move |content, _| {
            theme_settings::set_theme(content, theme_name, appearance, system_appearance);
        });
        Ok(())
    })
}
