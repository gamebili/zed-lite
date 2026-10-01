use anyhow::Result;
use edit_prediction_types::EditPredictionDelegateHandle;
use editor::{
    Editor, MultiBufferOffset, SelectionEffects, actions::ShowEditPrediction, scroll::Autoscroll,
};
use fs::Fs;
use gpui::{
    Action, Anchor, Animation, AnimationExt, App, AsyncWindowContext, Entity, FocusHandle,
    Focusable, IntoElement, ParentElement, Render, Subscription, TaskExt, WeakEntity, actions, div,
    pulsating_between,
};
use indoc::indoc;
use language::{
    EditPredictionsMode, File, Language,
    language_settings::{
        AllLanguageSettings, EditPredictionProvider, LanguageSettings, all_language_settings,
    },
};
use settings::{
    Settings, SettingsContent, SettingsStore, SplicingVec, find_value_range_in_json_text,
    update_settings_file,
};
use std::{ops::Range, sync::Arc, time::Duration};
use ui::{
    ContextMenu, ContextMenuEntry, DocumentationSide, IconButton, IconButtonShape, Indicator,
    PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;

use workspace::{
    HideStatusItem, StatusItemView, Workspace, create_and_open_local_file, item::ItemHandle,
};
use zed_actions::OpenSettingsAt;

actions!(
    edit_prediction,
    [
        /// Toggles the edit prediction menu.
        ToggleMenu
    ]
);

pub struct EditPredictionButton {
    editor_subscription: Option<(Subscription, usize)>,
    editor_enabled: Option<bool>,
    editor_show_predictions: bool,
    editor_focus_handle: Option<FocusHandle>,
    language: Option<Arc<Language>>,
    file: Option<Arc<dyn File>>,
    edit_prediction_provider: Option<Arc<dyn EditPredictionDelegateHandle>>,
    fs: Arc<dyn Fs>,
    popover_menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl Render for EditPredictionButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if all_language_settings(None, cx).edit_predictions.provider == EditPredictionProvider::None
        {
            return div().hidden();
        }

        let enabled = self.editor_enabled.unwrap_or(true);
        let show_editor_predictions = self.editor_show_predictions;
        let error = deepseek_status_error(cx);
        let indicator_color = if error.is_some() {
            Some(Color::Error)
        } else if !enabled || !show_editor_predictions {
            Some(Color::Ignored)
        } else {
            None
        };

        let icon_button = IconButton::new("deepseek-edit-prediction", IconName::AiDeepSeek)
            .shape(IconButtonShape::Square)
            .tab_index(0isize)
            .aria_label("Edit Prediction")
            .when_some(indicator_color, |this, color| {
                this.indicator(Indicator::dot().color(color))
                    .indicator_border_color(Some(cx.theme().colors().status_bar_background))
            })
            .when(!self.popover_menu_handle.is_deployed(), |element| {
                element.tooltip(move |_window, cx| {
                    let description: SharedString = if let Some(error) = &error {
                        error.clone()
                    } else if !enabled {
                        "Disabled For This File".into()
                    } else if !show_editor_predictions {
                        "Enable to Use".into()
                    } else {
                        "Powered by DeepSeek".into()
                    };
                    Tooltip::with_meta("Edit Prediction", Some(&ToggleMenu), description, cx)
                })
            });

        let this = cx.weak_entity();
        let popover_menu = PopoverMenu::new("edit-prediction")
            .menu(move |window, cx| {
                this.update(cx, |this, cx| this.build_deepseek_context_menu(window, cx))
                    .ok()
            })
            .anchor(Anchor::BottomRight)
            .with_handle(self.popover_menu_handle.clone());

        let is_refreshing = self
            .edit_prediction_provider
            .as_ref()
            .is_some_and(|provider| provider.is_refreshing(cx));
        let popover_menu = if is_refreshing {
            popover_menu.trigger(
                icon_button.with_animation(
                    "pulsating-label",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.2, 1.0)),
                    |icon_button, delta| icon_button.alpha(delta),
                ),
            )
        } else {
            popover_menu.trigger(icon_button)
        };

        div().child(popover_menu.into_any_element())
    }
}

/// Why DeepSeek predictions are unavailable: an unusable `key.enc` or the last failed request.
fn deepseek_status_error(cx: &mut App) -> Option<SharedString> {
    if let Err(error) = deepseek_edit_prediction::credentials() {
        return Some(error);
    }
    deepseek_edit_prediction::status(cx)
        .read(cx)
        .last_error
        .clone()
}

impl EditPredictionButton {
    pub fn new(
        fs: Arc<dyn Fs>,
        popover_menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_global::<SettingsStore>(move |_, cx| cx.notify())
            .detach();
        let status = deepseek_edit_prediction::status(cx);
        cx.observe(&status, |_, _, cx| cx.notify()).detach();

        Self {
            editor_subscription: None,
            editor_enabled: None,
            editor_show_predictions: true,
            editor_focus_handle: None,
            language: None,
            file: None,
            edit_prediction_provider: None,
            popover_menu_handle,
            fs,
        }
    }

    fn build_deepseek_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let error = deepseek_status_error(cx);
        let model = all_language_settings(None, cx)
            .edit_predictions
            .deepseek
            .model
            .clone();

        ContextMenu::build(window, cx, |mut menu, window, cx| {
            menu = menu.header(format!("DeepSeek · {model}"));
            if let Some(error) = error {
                menu = menu
                    .item(
                        ContextMenuEntry::new(error)
                            .disabled(true)
                            .icon(IconName::Warning)
                            .icon_color(Color::Error),
                    )
                    .separator();
            }

            menu = self.build_language_settings_menu(menu, window, cx);

            let fs = self.fs.clone();
            menu.separator()
                .entry("Disable Edit Predictions", None, move |_, cx| {
                    set_completion_provider(fs.clone(), cx, EditPredictionProvider::None);
                })
                .item(
                    ContextMenuEntry::new("Edit Prediction Settings")
                        .icon(IconName::Settings)
                        .icon_position(IconPosition::Start)
                        .icon_color(Color::Muted)
                        .handler(move |window, cx| {
                            window.dispatch_action(
                                OpenSettingsAt {
                                    path: "edit_predictions".to_string(),
                                    target: None,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        }),
                )
        })
    }

    pub fn build_language_settings_menu(
        &self,
        mut menu: ContextMenu,
        _window: &Window,
        cx: &mut App,
    ) -> ContextMenu {
        let fs = self.fs.clone();

        menu = menu.header("Show Edit Predictions For");

        let language_state = self.language.as_ref().map(|language| {
            (
                language.clone(),
                LanguageSettings::resolve(None, Some(&language.name()), cx).show_edit_predictions,
            )
        });

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            let entry = ContextMenuEntry::new("This Buffer")
                .toggleable(IconPosition::Start, self.editor_show_predictions)
                .action(Box::new(editor::actions::ToggleEditPrediction))
                .handler(move |window, cx| {
                    editor_focus_handle.dispatch_action(
                        &editor::actions::ToggleEditPrediction,
                        window,
                        cx,
                    );
                });

            match language_state.clone() {
                Some((language, false)) => {
                    menu = menu.item(entry.disabled(true).documentation_aside(
                        DocumentationSide::Left,
                        move |_cx| {
                            Label::new(format!(
                                "Edit predictions are disabled for {}",
                                language.name()
                            ))
                            .into_any_element()
                        },
                    ));
                }
                Some(_) | None => menu = menu.item(entry),
            }
        }

        if let Some((language, language_enabled)) = language_state {
            let fs = fs.clone();
            let language_name = language.name();

            menu = menu.toggleable_entry(
                language_name.clone(),
                language_enabled,
                IconPosition::Start,
                None,
                move |_, cx| {
                    toggle_show_edit_predictions_for_language(language.clone(), fs.clone(), cx)
                },
            );
        }

        let settings = AllLanguageSettings::get_global(cx);

        let globally_enabled = settings.show_edit_predictions(None, cx);
        let entry = ContextMenuEntry::new("All Files")
            .toggleable(IconPosition::Start, globally_enabled)
            .action(workspace::ToggleEditPrediction.boxed_clone())
            .handler(|window, cx| {
                window.dispatch_action(workspace::ToggleEditPrediction.boxed_clone(), cx)
            });
        menu = menu.item(entry);

        let current_mode = settings.edit_predictions_mode();
        let subtle_mode = matches!(current_mode, EditPredictionsMode::Subtle);
        let eager_mode = matches!(current_mode, EditPredictionsMode::Eager);

        menu = menu
                .separator()
                .header("Display Modes")
                .item(
                    ContextMenuEntry::new("Eager")
                        .toggleable(IconPosition::Start, eager_mode)
                        .documentation_aside(DocumentationSide::Left, move |_| {
                            Label::new("Display predictions inline when there are no language server completions available.").into_any_element()
                        })
                        .handler({
                            let fs = fs.clone();
                            move |_, cx| {
                                toggle_edit_prediction_mode(fs.clone(), EditPredictionsMode::Eager, cx)
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Subtle")
                        .toggleable(IconPosition::Start, subtle_mode)
                        .documentation_aside(DocumentationSide::Left, move |_| {
                            Label::new(concat!(
                                "Display predictions inline only when holding a modifier key (",
                                ui::alt_key_name!(),
                                " by default)."
                            ))
                            .into_any_element()
                        })
                        .handler({
                            let fs = fs.clone();
                            move |_, cx| {
                                toggle_edit_prediction_mode(fs.clone(), EditPredictionsMode::Subtle, cx)
                            }
                        }),
                );

        menu = menu.separator().item(
            ContextMenuEntry::new("Configure Excluded Files")
                .icon(IconName::Lock)
                .icon_color(Color::Muted)
                .documentation_aside(DocumentationSide::Left, |_| {
                    Label::new(indoc! {"
                        Open your settings to add sensitive paths for which Zed will never predict edits."})
                    .into_any_element()
                })
                .handler(move |window, cx| {
                    if let Some(workspace) = Workspace::for_window(window, cx) {
                        let workspace = workspace.downgrade();
                        window
                            .spawn(cx, async |cx| {
                                open_disabled_globs_setting_in_editor(workspace, cx).await
                            })
                            .detach_and_log_err(cx);
                    }
                }),
        );

        if !self.editor_enabled.unwrap_or(true) {
            menu = menu.item(
                ContextMenuEntry::new("This file is excluded.")
                    .disabled(true)
                    .icon(IconName::ZedPredictDisabled)
                    .icon_size(IconSize::Small),
            );
        }

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            menu = menu
                .separator()
                .header("Actions")
                .entry(
                    "Predict Edit at Cursor",
                    Some(Box::new(ShowEditPrediction)),
                    {
                        let editor_focus_handle = editor_focus_handle.clone();
                        move |window, cx| {
                            editor_focus_handle.dispatch_action(&ShowEditPrediction, window, cx);
                        }
                    },
                )
                .context(editor_focus_handle);
        }

        menu
    }

    pub fn update_enabled(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let suggestion_anchor = editor.selections.newest_anchor().start;
        let language = snapshot.language_at(suggestion_anchor);
        let file = snapshot.file_at(suggestion_anchor).cloned();
        self.editor_enabled = {
            let file = file.as_ref();
            Some(
                file.map(|file| {
                    all_language_settings(Some(file), cx)
                        .edit_predictions_enabled_for_file(file, cx)
                })
                .unwrap_or(true),
            )
        };
        self.editor_show_predictions = editor.edit_predictions_enabled();
        self.edit_prediction_provider = editor.edit_prediction_provider();
        self.language = language.cloned();
        self.file = file;
        self.editor_focus_handle = Some(editor.focus_handle(cx));

        cx.notify();
    }
}

impl StatusItemView for EditPredictionButton {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = item.and_then(|item| item.act_as::<Editor>(cx)) {
            self.editor_subscription = Some((
                cx.observe(&editor, Self::update_enabled),
                editor.entity_id().as_u64() as usize,
            ));
            self.update_enabled(editor, cx);
        } else {
            self.language = None;
            self.editor_subscription = None;
            self.editor_enabled = None;
        }
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // This button is already gated on having a non-disabled edit
        // prediction provider, which the user manages through provider/AI
        // settings.
        None
    }
}

fn initialize_disabled_globs_setting(file: &mut SettingsContent) {
    file.project
        .all_languages
        .edit_predictions
        .get_or_insert_with(Default::default)
        .disabled_globs
        .get_or_insert_with(|| SplicingVec::from(vec![SplicingVec::REST.to_string()]));
}

fn disabled_globs_content_range(text: &str) -> Option<Range<usize>> {
    let array_range = find_value_range_in_json_text(text, &["edit_predictions", "disabled_globs"])?;
    let content = text
        .get(array_range.clone())?
        .strip_prefix('[')?
        .strip_suffix(']')?;
    let start = array_range.start + 1 + (content.len() - content.trim_start().len());
    Some(start..start + content.trim().len())
}

async fn open_disabled_globs_setting_in_editor(
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let settings_editor = workspace
        .update_in(cx, |_, window, cx| {
            create_and_open_local_file(paths::settings_file(), window, cx, || {
                settings::initial_user_settings_content().as_ref().into()
            })
        })?
        .await?
        .downcast::<Editor>()
        .unwrap();

    settings_editor
        .downgrade()
        .update_in(cx, |item, window, cx| {
            let text = item.buffer().read(cx).snapshot(cx).text();

            let settings = cx.global::<SettingsStore>();

            let Some(edits) = settings
                .edits_for_update(&text, initialize_disabled_globs_setting)
                .log_err()
            else {
                return;
            };

            if !edits.is_empty() {
                item.edit(
                    edits
                        .into_iter()
                        .map(|(r, s)| (MultiBufferOffset(r.start)..MultiBufferOffset(r.end), s)),
                    cx,
                );
            }

            let text = item.buffer().read(cx).snapshot(cx).text();

            if let Some(range) = disabled_globs_content_range(&text) {
                let range = MultiBufferOffset(range.start)..MultiBufferOffset(range.end);
                item.change_selections(
                    SelectionEffects::scroll(Autoscroll::newest()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges(vec![range]);
                    },
                );
            }
        })?;

    anyhow::Ok(())
}

pub fn set_completion_provider(fs: Arc<dyn Fs>, cx: &mut App, provider: EditPredictionProvider) {
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .all_languages
            .edit_predictions
            .get_or_insert_default()
            .provider = Some(provider);
    });
}

fn toggle_show_edit_predictions_for_language(
    language: Arc<Language>,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) {
    let show_edit_predictions =
        all_language_settings(None, cx).show_edit_predictions(Some(&language), cx);
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .all_languages
            .languages
            .0
            .entry(language.name().0.to_string())
            .or_default()
            .show_edit_predictions = Some(!show_edit_predictions);
    });
}

fn toggle_edit_prediction_mode(fs: Arc<dyn Fs>, mode: EditPredictionsMode, cx: &mut App) {
    let settings = AllLanguageSettings::get_global(cx);
    let current_mode = settings.edit_predictions_mode();

    if current_mode != mode {
        update_settings_file(fs, cx, move |settings, _cx| {
            if let Some(edit_predictions) = settings.project.all_languages.edit_predictions.as_mut()
            {
                edit_predictions.mode = Some(mode);
            } else {
                settings.project.all_languages.edit_predictions =
                    Some(settings::EditPredictionSettingsContent {
                        mode: Some(mode),
                        ..Default::default()
                    });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use settings::RootUserSettings;

    #[test]
    fn test_disabled_globs_selection_and_replacement() -> Result<()> {
        for contents in [
            r#""[.][.][.]""#,
            r#""**/[ab]/**", "...""#,
            r#""escaped\"]", "backslash\\", "...""#,
            r#""escaped\\]", "line\nbreak]", "unicode\u005d""#,
            "\n    \"é[ab]\",\n    \"...\"\n  ",
            "// Ignore filenames containing a \" character\n    \"**/private/**\"\n",
            "/* Ignore \" and ] */\n    \"**/private/**\"\n",
            "\"**/private/**\", // Keep \" and ]\n    \"...\"\n",
            r#""**/build/**", "...""#,
            "",
            "\n  ",
        ] {
            let mut text = format!(
                r#"// "disabled_globs": ["commented/**"]
{{"languages":{{"Rust":{{"disabled_globs":["nested/**"]}}}},
/* "disabled_globs": ["commented/**"] */
"edit_predictions":{{"disabled_globs":[{contents}],"mode":"subtle"}}}}"#
            );
            let mut expected = SettingsContent::parse_json_with_comments(&text)?;
            let range = disabled_globs_content_range(&text).context("disabled globs selection")?;
            assert_eq!(text.get(range.clone()), Some(contents.trim()));

            text.replace_range(range, r#""**/replacement/**", "...""#);
            expected
                .project
                .all_languages
                .edit_predictions
                .as_mut()
                .context("edit prediction settings")?
                .disabled_globs = Some(SplicingVec::from(vec![
                "**/replacement/**".to_string(),
                SplicingVec::REST.to_string(),
            ]));
            assert_eq!(SettingsContent::parse_json_with_comments(&text)?, expected);
        }
        Ok(())
    }

    #[gpui::test]
    fn test_initialize_disabled_globs_setting(cx: &mut App) {
        let store = SettingsStore::new(cx, &settings::default_settings());

        let cases: &[(&str, &[&str])] = &[
            ("", &[SplicingVec::REST]),
            (r#"{}"#, &[SplicingVec::REST]),
            (r#"{"edit_predictions":{}}"#, &[SplicingVec::REST]),
            (
                r#"{"edit_predictions":{"mode":"subtle"}}"#,
                &[SplicingVec::REST],
            ),
            (r#"{"edit_predictions":{"disabled_globs":[]}}"#, &[]),
            (
                r#"{"edit_predictions":{"disabled_globs":["**/build/**"]}}"#,
                &["**/build/**"],
            ),
            (
                r#"{"edit_predictions":{"disabled_globs":["...","**/build/**"]}}"#,
                &[SplicingVec::REST, "**/build/**"],
            ),
            (
                r#"{"edit_predictions":{"disabled_globs":["[.][.][.]"]}}"#,
                &["[.][.][.]"],
            ),
        ];
        for &(content, expected_globs) in cases {
            let original = if content.is_empty() { "{}" } else { content };
            let mut expected_content = SettingsContent::parse_json_with_comments(original)
                .expect("settings content parses");
            let original_globs = &mut expected_content
                .project
                .all_languages
                .edit_predictions
                .get_or_insert_with(Default::default)
                .disabled_globs;
            let already_configured = original_globs.is_some();
            *original_globs = Some(SplicingVec::from(
                expected_globs
                    .iter()
                    .map(|glob| glob.to_string())
                    .collect::<Vec<_>>(),
            ));

            let edits = store
                .edits_for_update(content, initialize_disabled_globs_setting)
                .expect("settings edits are generated");
            assert_eq!(edits.is_empty(), already_configured, "{content}");
            let mut updated = content.to_string();
            for (range, replacement) in edits {
                updated.replace_range(range, &replacement);
            }
            assert_eq!(
                SettingsContent::parse_json_with_comments(&updated)
                    .expect("updated settings parse"),
                expected_content,
                "{content}",
            );
            assert!(
                store
                    .edits_for_update(&updated, initialize_disabled_globs_setting)
                    .expect("repeated settings edits are generated")
                    .is_empty(),
                "{content}",
            );
        }
    }
}
