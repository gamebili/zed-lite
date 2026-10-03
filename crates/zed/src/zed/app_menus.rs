use gpui::{App, Menu, MenuItem, OsAction};
use release_channel::ReleaseChannel;
use terminal_view::terminal_panel;
use zed_actions::{Quit, debug_panel, dev, git_panel, project_panel};

pub fn app_menus(cx: &mut App) -> Vec<Menu> {
    let mut view_items = vec![
        MenuItem::action(
            ui::tr("Zoom In"),
            zed_actions::IncreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            ui::tr("Zoom Out"),
            zed_actions::DecreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            ui::tr("Reset Zoom"),
            zed_actions::ResetBufferFontSize { persist: false },
        ),
        MenuItem::action(
            ui::tr("Reset All Zoom"),
            zed_actions::ResetAllZoom { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action(ui::tr("Toggle Left Dock"), workspace::ToggleLeftDock),
        MenuItem::action(ui::tr("Toggle Right Dock"), workspace::ToggleRightDock),
        MenuItem::action(ui::tr("Toggle Bottom Dock"), workspace::ToggleBottomDock),
        MenuItem::action(ui::tr("Toggle All Docks"), workspace::ToggleAllDocks),
        MenuItem::submenu(Menu {
            name: ui::tr("Editor Layout"),
            disabled: false,
            items: vec![
                MenuItem::action(ui::tr("Split Up"), workspace::SplitUp::default()),
                MenuItem::action(ui::tr("Split Down"), workspace::SplitDown::default()),
                MenuItem::action(ui::tr("Split Left"), workspace::SplitLeft::default()),
                MenuItem::action(ui::tr("Split Right"), workspace::SplitRight::default()),
            ],
        }),
        MenuItem::separator(),
        MenuItem::action(ui::tr("Project Panel"), project_panel::ToggleFocus),
        MenuItem::action(ui::tr("Outline Panel"), outline_panel::ToggleFocus),
        MenuItem::action(ui::tr("Terminal Panel"), terminal_panel::Toggle),
        MenuItem::action(ui::tr("Debugger Panel"), debug_panel::ToggleFocus),
    ];

    view_items.extend([
        MenuItem::action(ui::tr("Git Panel"), git_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::action(ui::tr("Diagnostics"), diagnostics::Deploy),
        MenuItem::separator(),
    ]);

    if ReleaseChannel::try_global(cx) == Some(ReleaseChannel::Dev) {
        view_items.push(MenuItem::action(
            ui::tr("Toggle GPUI Inspector"),
            dev::ToggleInspector,
        ));
        view_items.push(MenuItem::separator());
    }

    vec![
        Menu {
            name: ui::tr("Zed"),
            disabled: false,
            items: vec![
                MenuItem::action(ui::tr("About Zed"), zed_actions::About),
                MenuItem::action(ui::tr("Check for Updates"), auto_update::Check),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new(ui::tr("Settings")).items([
                    MenuItem::action(ui::tr("Open Settings"), zed_actions::OpenSettings),
                    MenuItem::action(ui::tr("Open Settings File"), super::OpenSettingsFile),
                    MenuItem::action(
                        ui::tr("Open Project Settings"),
                        zed_actions::OpenProjectSettings,
                    ),
                    MenuItem::action(
                        ui::tr("Open Project Settings File"),
                        super::OpenProjectSettingsFile,
                    ),
                    MenuItem::action(ui::tr("Open Default Settings"), super::OpenDefaultSettings),
                    MenuItem::separator(),
                    MenuItem::action(ui::tr("Open Keymap"), zed_actions::OpenKeymap),
                    MenuItem::action(ui::tr("Open Keymap File"), zed_actions::OpenKeymapFile),
                    MenuItem::action(
                        ui::tr("Open Default Key Bindings"),
                        zed_actions::OpenDefaultKeymap,
                    ),
                    MenuItem::separator(),
                    MenuItem::action(
                        ui::tr("Select Theme..."),
                        zed_actions::theme_selector::Toggle::default(),
                    ),
                    MenuItem::action(
                        ui::tr("Select Icon Theme..."),
                        zed_actions::icon_theme_selector::Toggle::default(),
                    ),
                ])),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::os_submenu(ui::tr("Services"), gpui::SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Extensions"), zed_actions::Extensions::default()),
                #[cfg(not(target_os = "windows"))]
                MenuItem::action(ui::tr("Install CLI"), install_cli::InstallCliBinary),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::action(ui::tr("Hide Zed"), super::Hide),
                #[cfg(target_os = "macos")]
                MenuItem::action(ui::tr("Hide Others"), super::HideOthers),
                #[cfg(target_os = "macos")]
                MenuItem::action(ui::tr("Show All"), super::ShowAll),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Quit Zed"), Quit),
            ],
        },
        Menu {
            name: ui::tr("File"),
            disabled: false,
            items: vec![
                MenuItem::action(ui::tr("New"), workspace::NewFile),
                MenuItem::action(ui::tr("New Window"), workspace::NewWindow),
                MenuItem::separator(),
                #[cfg(not(target_os = "macos"))]
                MenuItem::action(ui::tr("Open File..."), workspace::OpenFiles),
                MenuItem::action(
                    if cfg!(not(target_os = "macos")) {
                        ui::tr("Open Folder...")
                    } else {
                        ui::tr("Open…")
                    },
                    workspace::Open::default(),
                ),
                MenuItem::action(ui::tr("Open Recent…"), zed_actions::OpenRecent::default()),
                MenuItem::action(ui::tr("Open Remote…"), zed_actions::OpenRemote::default()),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Add Folder to Project…"),
                    workspace::AddFolderToProject,
                ),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Save"), workspace::Save { save_intent: None }),
                MenuItem::action(ui::tr("Save As…"), workspace::SaveAs),
                MenuItem::action(ui::tr("Save All"), workspace::SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Close Editor"),
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action(ui::tr("Close Project"), workspace::CloseProject),
                MenuItem::action(ui::tr("Close Window"), workspace::CloseWindow),
            ],
        },
        Menu {
            name: ui::tr("Edit"),
            disabled: false,
            items: vec![
                MenuItem::os_action(ui::tr("Undo"), editor::actions::Undo, OsAction::Undo),
                MenuItem::os_action(ui::tr("Redo"), editor::actions::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action(ui::tr("Cut"), editor::actions::Cut, OsAction::Cut),
                MenuItem::os_action(ui::tr("Copy"), editor::actions::Copy, OsAction::Copy),
                MenuItem::action(ui::tr("Copy and Trim"), editor::actions::CopyAndTrim),
                MenuItem::os_action(ui::tr("Paste"), editor::actions::Paste, OsAction::Paste),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Find"), search::buffer_search::Deploy::find()),
                MenuItem::action(
                    ui::tr("Find in Project"),
                    workspace::DeploySearch::default(),
                ),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Toggle Line Comment"),
                    editor::actions::ToggleComments::default(),
                ),
            ],
        },
        Menu {
            name: ui::tr("Selection"),
            disabled: false,
            items: vec![
                MenuItem::os_action(
                    ui::tr("Select All"),
                    editor::actions::SelectAll,
                    OsAction::SelectAll,
                ),
                MenuItem::action(
                    ui::tr("Expand Selection"),
                    editor::actions::SelectLargerSyntaxNode,
                ),
                MenuItem::action(
                    ui::tr("Shrink Selection"),
                    editor::actions::SelectSmallerSyntaxNode,
                ),
                MenuItem::action(
                    ui::tr("Select Next Sibling"),
                    editor::actions::SelectNextSyntaxNode,
                ),
                MenuItem::action(
                    ui::tr("Select Previous Sibling"),
                    editor::actions::SelectPreviousSyntaxNode,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Add Cursor Above"),
                    editor::actions::AddSelectionAbove {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    ui::tr("Add Cursor Below"),
                    editor::actions::AddSelectionBelow {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    ui::tr("Select Next Occurrence"),
                    editor::actions::SelectNext {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    ui::tr("Select Previous Occurrence"),
                    editor::actions::SelectPrevious {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    ui::tr("Select All Occurrences"),
                    editor::actions::SelectAllMatches,
                ),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Move Line Up"), editor::actions::MoveLineUp),
                MenuItem::action(ui::tr("Move Line Down"), editor::actions::MoveLineDown),
                MenuItem::action(
                    ui::tr("Duplicate Selection"),
                    editor::actions::DuplicateLineDown,
                ),
            ],
        },
        Menu {
            name: ui::tr("View"),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: ui::tr("Go"),
            disabled: false,
            items: vec![
                MenuItem::action(ui::tr("Back"), workspace::GoBack),
                MenuItem::action(ui::tr("Forward"), workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Command Palette..."),
                    zed_actions::command_palette::Toggle,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Go to File..."),
                    workspace::ToggleFileFinder::default(),
                ),
                // MenuItem::action("Go to Symbol in Project", project_symbols::Toggle),
                MenuItem::action(
                    ui::tr("Go to Symbol in Editor..."),
                    zed_actions::outline::ToggleOutline,
                ),
                MenuItem::action(
                    ui::tr("Go to Line/Column..."),
                    editor::actions::ToggleGoToLine,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Go to Definition"),
                    editor::actions::GoToDefinition::default(),
                ),
                MenuItem::action(
                    ui::tr("Go to Declaration"),
                    editor::actions::GoToDeclaration::default(),
                ),
                MenuItem::action(
                    ui::tr("Go to Type Definition"),
                    editor::actions::GoToTypeDefinition::default(),
                ),
                MenuItem::action(
                    ui::tr("Find All References"),
                    editor::actions::FindAllReferences::default(),
                ),
                MenuItem::action(
                    ui::tr("Show Incoming Calls"),
                    call_hierarchy::ShowIncomingCalls,
                ),
                MenuItem::action(
                    ui::tr("Show Outgoing Calls"),
                    call_hierarchy::ShowOutgoingCalls,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Next Problem"),
                    editor::actions::GoToDiagnostic::default(),
                ),
                MenuItem::action(
                    ui::tr("Previous Problem"),
                    editor::actions::GoToPreviousDiagnostic::default(),
                ),
            ],
        },
        Menu {
            name: ui::tr("Run"),
            disabled: false,
            items: vec![
                MenuItem::action(
                    ui::tr("Spawn Task"),
                    zed_actions::Spawn::ViaModal {
                        reveal_target: None,
                    },
                ),
                MenuItem::action(ui::tr("Start Debugger"), debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Edit tasks.json…"), zed_actions::OpenProjectTasks),
                MenuItem::action(
                    ui::tr("Edit debug.json…"),
                    zed_actions::OpenProjectDebugTasks,
                ),
                MenuItem::separator(),
                MenuItem::action(ui::tr("Continue"), debugger_ui::Continue),
                MenuItem::action(ui::tr("Step Over"), debugger_ui::StepOver),
                MenuItem::action(ui::tr("Step Into"), debugger_ui::StepInto),
                MenuItem::action(ui::tr("Step Out"), debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Toggle Breakpoint"),
                    editor::actions::ToggleBreakpoint,
                ),
                MenuItem::action(
                    ui::tr("Edit Breakpoint"),
                    editor::actions::EditLogBreakpoint,
                ),
                MenuItem::action(
                    ui::tr("Clear All Breakpoints"),
                    debugger_ui::ClearAllBreakpoints,
                ),
            ],
        },
        Menu {
            name: ui::tr("Window"),
            disabled: false,
            items: vec![
                MenuItem::action(ui::tr("Minimize"), super::Minimize),
                MenuItem::action(ui::tr("Zoom"), super::Zoom),
                MenuItem::separator(),
            ],
        },
        Menu {
            name: ui::tr("Help"),
            disabled: false,
            items: vec![
                MenuItem::action(
                    ui::tr("View Release Notes Locally"),
                    auto_update_ui::ViewReleaseNotesLocally,
                ),
                MenuItem::action(ui::tr("View Telemetry"), zed_actions::OpenTelemetryLog),
                MenuItem::action(
                    ui::tr("View Dependency Licenses"),
                    zed_actions::OpenLicenses,
                ),
                MenuItem::action(ui::tr("Show Welcome"), onboarding::ShowWelcome),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("File Bug Report..."),
                    zed_actions::feedback::FileBugReport,
                ),
                MenuItem::action(
                    ui::tr("Request Feature..."),
                    zed_actions::feedback::RequestFeature,
                ),
                MenuItem::action(ui::tr("Email Us..."), zed_actions::feedback::EmailZed),
                MenuItem::separator(),
                MenuItem::action(
                    ui::tr("Documentation"),
                    super::OpenBrowser {
                        url: "https://zed.dev/docs".into(),
                    },
                ),
                MenuItem::action(ui::tr("Zed Repository"), feedback::OpenZedRepo),
                MenuItem::action(
                    ui::tr("Zed Twitter"),
                    super::OpenBrowser {
                        url: "https://twitter.com/zeddotdev".into(),
                    },
                ),
                MenuItem::action(
                    ui::tr("Join the Team"),
                    super::OpenBrowser {
                        url: "https://zed.dev/jobs".into(),
                    },
                ),
            ],
        },
    ]
}
