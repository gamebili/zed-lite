use client::Client;
use collections::HashMap;
use deepseek_edit_prediction::DeepSeekEditPredictionDelegate;
use editor::{EditPredictionRequestTrigger, Editor};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, WeakEntity};
use language::language_settings::{EditPredictionProvider, all_language_settings};

use settings::SettingsStore;
use std::{cell::RefCell, rc::Rc, sync::Arc};
use ui::Window;

pub fn init(client: Arc<Client>, cx: &mut App) {
    let editors: Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>> = Rc::default();
    cx.observe_new({
        let editors = editors.clone();
        let client = client.clone();
        move |editor: &mut Editor, window, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }

            let Some(window) = window else {
                return;
            };

            let editor_handle = cx.entity().downgrade();
            cx.on_release({
                let editor_handle = editor_handle.clone();
                let editors = editors.clone();
                move |_, _| {
                    editors.borrow_mut().remove(&editor_handle);
                }
            })
            .detach();

            editors
                .borrow_mut()
                .insert(editor_handle, window.window_handle());
            assign_edit_prediction_provider(
                editor,
                edit_prediction_provider_for_settings(cx),
                EditPredictionRequestTrigger::EditorCreated,
                &client,
                window,
                cx,
            );
        }
    })
    .detach();

    cx.observe_global::<SettingsStore>({
        let mut previous_provider = edit_prediction_provider_for_settings(cx);
        move |cx| {
            let new_provider = edit_prediction_provider_for_settings(cx);
            if new_provider != previous_provider {
                previous_provider = new_provider;
                assign_edit_prediction_providers(
                    &editors,
                    new_provider,
                    EditPredictionRequestTrigger::ProviderChanged,
                    &client,
                    cx,
                );
            }
        }
    })
    .detach();
}

fn edit_prediction_provider_for_settings(cx: &App) -> EditPredictionProvider {
    all_language_settings(None, cx).edit_predictions.provider
}

fn assign_edit_prediction_providers(
    editors: &Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>>,
    provider: EditPredictionProvider,
    trigger: EditPredictionRequestTrigger,
    client: &Arc<Client>,
    cx: &mut App,
) {
    for (editor, window) in editors.borrow().iter() {
        _ = window.update(cx, |_window, window, cx| {
            _ = editor.update(cx, |editor, cx| {
                assign_edit_prediction_provider(editor, provider, trigger, client, window, cx);
            })
        });
    }
}

fn assign_edit_prediction_provider(
    editor: &mut Editor,
    provider: EditPredictionProvider,
    trigger: EditPredictionRequestTrigger,
    client: &Arc<Client>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    match provider {
        EditPredictionProvider::None => {
            editor.set_edit_prediction_provider::<DeepSeekEditPredictionDelegate>(
                None, trigger, window, cx,
            );
        }
        EditPredictionProvider::DeepSeek => {
            let http_client = client.http_client();
            let provider = cx.new(|cx| DeepSeekEditPredictionDelegate::new(http_client, cx));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::MultiBuffer;
    use gpui::{BorrowAppContext, TestAppContext};
    use settings::SettingsStore;
    use workspace::AppState;

    fn set_provider(provider: EditPredictionProvider, cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.edit_predictions =
                        Some(settings::EditPredictionSettingsContent {
                            provider: Some(provider),
                            ..Default::default()
                        });
                });
            });
        });
    }

    #[gpui::test]
    async fn test_provider_follows_settings(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            app_state
        });
        set_provider(EditPredictionProvider::None, cx);
        cx.update(|cx| init(app_state.client.clone(), cx));

        let editor = cx.add_window(|window, cx| {
            let buffer = cx.new(|_cx| MultiBuffer::new(language::Capability::ReadWrite));
            Editor::new(editor::EditorMode::full(), buffer, None, window, cx)
        });
        editor
            .update(cx, |editor, _window, _cx| {
                assert!(editor.edit_prediction_provider().is_none());
            })
            .unwrap();

        set_provider(EditPredictionProvider::DeepSeek, cx);
        editor
            .update(cx, |editor, _window, _cx| {
                assert!(editor.edit_prediction_provider().is_some());
            })
            .unwrap();

        set_provider(EditPredictionProvider::None, cx);
        editor
            .update(cx, |editor, _window, _cx| {
                assert!(editor.edit_prediction_provider().is_none());
            })
            .unwrap();
    }
}
