mod playback;

use std::{
    collections::HashMap,
    io::Read as _,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use async_lock::Mutex;
use file_icons::FileIcons;
pub use file_preview::run_decoder_worker_if_invoked;
use file_preview::{PreviewPage, PreviewRequest};
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Global, Image, ImageFormat, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, SharedString, Task, UniformListScrollHandle, WeakEntity,
    Window, img, px, uniform_list,
};
use language::{ByteContent, Capability, FILE_ANALYSIS_BYTES};
use project::{Project, ProjectEntryId, ProjectPath};
use ui::{ListItem, Tooltip, prelude::*};
use workspace::{
    ItemId, Pane, Workspace, WorkspaceId, delete_unloaded_items,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SerializableItem},
};

use persistence::FileViewerDb;

const CELL_WIDTH: f32 = 220.0;
const HEX_COLUMN_WIDTH: f32 = 480.0;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 1024 * 1024;
const MAX_PAGE_HISTORY: usize = 4096;

#[derive(Default)]
struct ReadGate(Arc<Mutex<()>>);

impl Global for ReadGate {}

#[derive(Default)]
struct ImageOwners(HashMap<u64, usize>);

impl Global for ImageOwners {}

pub struct FileItem {
    project: WeakEntity<Project>,
    original_project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    original_absolute_path: Option<PathBuf>,
    local: bool,
    read_gate: Arc<Mutex<()>>,
}

impl FileItem {
    fn absolute_path(&self, cx: &App) -> Option<PathBuf> {
        self.project
            .upgrade()
            .and_then(|project| {
                project
                    .read(cx)
                    .absolute_path(&self.current_project_path(cx), cx)
            })
            .or_else(|| self.original_absolute_path.clone())
    }

    fn current_project_path(&self, cx: &App) -> ProjectPath {
        self.project
            .upgrade()
            .and_then(|project| project.read(cx).path_for_entry(self.entry_id?, cx))
            .unwrap_or_else(|| self.original_project_path.clone())
    }

    fn new(project: &Entity<Project>, path: &ProjectPath, cx: &mut App) -> Entity<Self> {
        let original_absolute_path = project.read(cx).absolute_path(path, cx);
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let local = project.read(cx).is_local();
        let read_gate = cx.default_global::<ReadGate>().0.clone();
        cx.new(|_| Self {
            project: project.downgrade(),
            original_project_path: path.clone(),
            entry_id,
            original_absolute_path,
            local,
            read_gate,
        })
    }
}

impl project::ProjectItem for FileItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        file_preview::classify(path.path.as_std_path())?;
        Some(Task::ready(Ok(Self::new(project, path, cx))))
    }

    fn try_open_async(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Task<Result<Option<Entity<Self>>>> {
        if let Some(task) = Self::try_open(project, path, cx) {
            return cx.spawn(async move |_| task.await.map(Some));
        }
        let project_state = project.read(cx);
        if !project_state.is_local()
            || project_state
                .entry_for_path(path, cx)
                .is_some_and(|entry| !entry.is_file())
        {
            return Task::ready(Ok(None));
        }
        let Some(absolute_path) = project_state.absolute_path(path, cx) else {
            return Task::ready(Ok(None));
        };
        let filesystem = project_state.fs().clone();
        let project = project.clone();
        let path = path.clone();
        let probe = cx.background_spawn(async move {
            let Some(metadata) = filesystem
                .metadata(&absolute_path)
                .await
                .with_context(|| format!("Inspecting file {}", absolute_path.display()))?
            else {
                return Ok(false);
            };
            if metadata.is_dir || metadata.is_fifo {
                return Ok(false);
            }
            let reader = match filesystem.open_sync(&absolute_path).await {
                Ok(reader) => reader,
                Err(error)
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    }) =>
                {
                    return Ok(false);
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Inspecting file {}", absolute_path.display()));
                }
            };
            let mut prefix = Vec::with_capacity(FILE_ANALYSIS_BYTES);
            reader
                .take(FILE_ANALYSIS_BYTES as u64)
                .read_to_end(&mut prefix)
                .with_context(|| format!("Reading file prefix {}", absolute_path.display()))?;
            Ok(worktree::decode_byte_header(&prefix).1 == ByteContent::Binary)
        });
        cx.spawn(async move |cx| {
            if probe.await? {
                Ok(Some(cx.update(|cx| Self::new(&project, &path, cx))))
            } else {
                Ok(None)
            }
        })
    }

    fn entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        self.project
            .upgrade()
            .and_then(|project| {
                project
                    .read(cx)
                    .entry_for_path(&self.current_project_path(cx), cx)
                    .map(|entry| entry.id)
            })
            .or(self.entry_id)
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.current_project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct PageNavigation {
    section: Option<String>,
    bytes: bool,
    offset: u64,
    previous_offsets: Vec<u64>,
    page_number: u64,
}

impl PageNavigation {
    fn request(&self) -> PreviewRequest {
        PreviewRequest {
            section: self.section.clone(),
            offset: self.offset,
        }
    }

    fn select_section(&mut self, section: Option<String>) {
        self.bytes = false;
        self.section = section;
        self.reset_page();
    }

    fn reset_page(&mut self) {
        self.offset = 0;
        self.previous_offsets.clear();
        self.page_number = 0;
    }

    fn toggle_bytes(&mut self) {
        self.bytes = !self.bytes;
        self.reset_page();
    }

    fn next(&mut self, offset: u64) {
        if self.previous_offsets.len() == MAX_PAGE_HISTORY {
            self.previous_offsets.remove(0);
        }
        self.previous_offsets.push(self.offset);
        self.offset = offset;
        self.page_number = self.page_number.saturating_add(1);
    }

    fn previous(&mut self) -> bool {
        if let Some(offset) = self.previous_offsets.pop() {
            self.offset = offset;
            self.page_number = self.page_number.saturating_sub(1);
            true
        } else {
            false
        }
    }
}

pub struct FileView {
    item: Entity<FileItem>,
    focus_handle: FocusHandle,
    navigation: PageNavigation,
    page: Option<Arc<PreviewPage>>,
    image: Option<Arc<Image>>,
    sections: Arc<Vec<String>>,
    loading: bool,
    error: Option<SharedString>,
    generation: u64,
    cancellation: Option<Arc<AtomicBool>>,
    read_task: Option<Task<()>>,
    row_scroll: UniformListScrollHandle,
    section_scroll: UniformListScrollHandle,
    metadata_scroll: UniformListScrollHandle,
    selected_cell: Option<(usize, usize)>,
    copied_page: bool,
    playback: Option<playback::Playback>,
    playback_task: Option<Task<()>>,
    playback_paused: bool,
    playback_starting: bool,
    playback_message: Option<SharedString>,
    automatic_frames: bool,
    frame_task: Option<Task<()>>,
}

impl FileView {
    fn unloaded(item: Entity<FileItem>, cx: &mut Context<Self>) -> Self {
        Self {
            item,
            focus_handle: cx.focus_handle(),
            navigation: PageNavigation::default(),
            page: None,
            image: None,
            sections: Arc::new(Vec::new()),
            loading: false,
            error: None,
            generation: 0,
            cancellation: None,
            read_task: None,
            row_scroll: UniformListScrollHandle::new(),
            section_scroll: UniformListScrollHandle::new(),
            metadata_scroll: UniformListScrollHandle::new(),
            selected_cell: None,
            copied_page: false,
            playback: None,
            playback_task: None,
            playback_paused: false,
            playback_starting: false,
            playback_message: None,
            automatic_frames: false,
            frame_task: None,
        }
    }

    fn new(item: Entity<FileItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_navigation(item, PageNavigation::default(), window, cx)
    }

    fn with_navigation(
        item: Entity<FileItem>,
        navigation: PageNavigation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(project) = item.read(cx).project.upgrade() {
            cx.subscribe(&project, |_, _, event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeUpdatedEntries(..)
                        | project::Event::DeletedEntry(..)
                        | project::Event::WorktreePathsChanged { .. }
                ) {
                    cx.emit(ItemEvent::UpdateTab);
                    cx.emit(ItemEvent::UpdateBreadcrumbs);
                    cx.notify();
                }
            })
            .detach();
        }
        let window_handle = window.window_handle();
        cx.on_release(move |this, cx| {
            this.cancel_read();
            this.stop_media();
            if window_handle
                .update(cx, |_, window, cx| this.release_image(window, cx))
                .is_ok()
            {
                return;
            }
            // Closing a window can precede releasing its views. Asset ownership must
            // still be released, and a remaining window can evict the shared atlas entry.
            for window_handle in cx.windows() {
                if window_handle
                    .update(cx, |_, window, cx| this.release_image(window, cx))
                    .is_ok()
                {
                    return;
                }
            }
            if let Some(image) = this.take_last_owned_image(cx) {
                image.remove_asset(cx);
            }
        })
        .detach();
        let mut this = Self::unloaded(item, cx);
        this.navigation = navigation;
        this.load(window, cx);
        this
    }

    fn cancel_read(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.read_task = None;
    }

    fn take_last_owned_image(&mut self, cx: &mut App) -> Option<Arc<Image>> {
        self.image.take().and_then(|image| {
            let last_owner = cx.update_default_global(|owners: &mut ImageOwners, _| {
                let Some(count) = owners.0.get_mut(&image.id()) else {
                    return true;
                };
                if *count > 1 {
                    *count -= 1;
                    false
                } else {
                    owners.0.remove(&image.id());
                    true
                }
            });
            last_owner.then_some(image)
        })
    }

    fn release_image(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(image) = self.take_last_owned_image(cx) {
            if let Some(render_image) = image.clone().get_render_image(window, cx) {
                cx.drop_image(render_image, Some(window));
            }
            image.remove_asset(cx);
        }
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_media();
        self.load_page(window, cx);
    }

    fn load_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_read();
        self.generation = self.generation.wrapping_add(1);
        self.page = None;
        self.release_image(window, cx);
        self.selected_cell = None;
        self.copied_page = false;
        self.row_scroll = UniformListScrollHandle::new();
        self.metadata_scroll = UniformListScrollHandle::new();
        self.error = None;

        let item = self.item.read(cx);
        if !item.local {
            self.loading = false;
            self.error = Some(
                "This read-only viewer needs a local file. Remote binary previews are not available."
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(path) = item.absolute_path(cx) else {
            self.loading = false;
            self.error = Some("The file path is no longer available.".into());
            cx.notify();
            return;
        };

        let read_gate = item.read_gate.clone();
        let request = self.navigation.request();
        let bytes = self.navigation.bytes;
        let generation = self.generation;
        let cancellation = Arc::new(AtomicBool::new(false));
        self.cancellation = Some(cancellation.clone());
        self.loading = true;

        // Dropping a task cannot interrupt synchronous parsers. Keep their work serialized
        // even after a tab is closed, and skip obsolete requests before entering a parser.
        let read = cx.background_spawn(async move {
            let _guard = read_gate.lock().await;
            if cancellation.load(Ordering::Acquire) {
                return None;
            }
            let result = if bytes {
                file_preview::read_bytes(&path, &request)
            } else {
                file_preview::read(&path, &request)
            };
            (!cancellation.load(Ordering::Acquire)).then_some(result)
        });
        self.read_task = Some(cx.spawn(async move |this, cx| {
            let Some(result) = read.await else {
                return;
            };
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.apply_result(generation, result, cx));
            }
        }));
        cx.notify();
    }

    fn apply_result(
        &mut self,
        generation: u64,
        result: Result<PreviewPage>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        self.cancellation = None;
        match result.and_then(validate_image) {
            Ok(mut page) => {
                if page.is_hex && !self.navigation.bytes {
                    self.stop_media();
                    self.navigation.bytes = true;
                    self.navigation.reset_page();
                }
                if !self.navigation.bytes && self.navigation.section.is_none() {
                    self.navigation.section = page.sections.first().cloned();
                }
                if !page.sections.is_empty() {
                    self.sections = Arc::new(std::mem::take(&mut page.sections));
                }
                self.image = page.image.take().map(|bytes| {
                    let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
                    *cx.default_global::<ImageOwners>()
                        .0
                        .entry(image.id())
                        .or_default() += 1;
                    image
                });
                self.page = Some(Arc::new(page));
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
            }
        }
        cx.notify();
    }

    fn select_section(&mut self, section: String, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation.select_section(Some(section));
        self.load(window, cx);
    }

    fn toggle_bytes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation.toggle_bytes();
        self.load(window, cx);
    }

    fn previous_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loading && self.navigation.previous() {
            self.load(window, cx);
        }
    }

    fn next_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loading
            && let Some(offset) = self.page.as_ref().and_then(|page| page.next_offset)
        {
            self.navigation.next(offset);
            self.load(window, cx);
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation.reset_page();
        self.load(window, cx);
    }

    fn copy_page(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = &self.page {
            cx.write_to_clipboard(ClipboardItem::new_string(page_as_tsv(page)));
            self.copied_page = true;
            cx.notify();
        }
    }

    fn media_kind(&self, cx: &App) -> Option<file_preview::FileKind> {
        let path = self.item.read(cx).current_project_path(cx);
        file_preview::classify(path.path.as_std_path()).filter(|kind| {
            matches!(
                kind,
                file_preview::FileKind::Audio | file_preview::FileKind::Video
            )
        })
    }

    fn stop_media(&mut self) {
        self.playback_task = None;
        self.playback = None;
        self.playback_paused = false;
        self.playback_starting = false;
        self.playback_message = None;
        self.automatic_frames = false;
        self.frame_task = None;
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if let Some(playback) = &self.playback {
            let result = if self.playback_paused {
                playback.play()
            } else {
                playback.pause()
            };
            if let Err(error) = result {
                self.playback_message = Some(format!("{error:#}").into());
            }
            cx.notify();
            return;
        }
        let item = self.item.read(cx);
        if !item.local {
            self.playback_message = Some("Playback needs a local file.".into());
            cx.notify();
            return;
        }
        let Some(path) = item.absolute_path(cx) else {
            self.playback_message = Some("The file path is no longer available.".into());
            cx.notify();
            return;
        };
        match playback::Playback::start(path, self.navigation.offset) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.playback_starting = true;
                self.playback_paused = false;
                self.playback_message = Some("Starting playback…".into());
                self.playback_task = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                        let Some(this) = this.upgrade() else {
                            return;
                        };
                        let continue_polling = this.update(cx, |this, cx| {
                            let status = this.playback.as_mut().and_then(playback::Playback::poll);
                            match status {
                                Some(playback::Status::Playing) => {
                                    this.playback_starting = false;
                                    this.playback_paused = false;
                                    this.playback_message = Some("Playing".into());
                                }
                                Some(playback::Status::Paused) => {
                                    this.playback_starting = false;
                                    this.playback_paused = true;
                                    this.playback_message = Some("Paused".into());
                                }
                                Some(playback::Status::Ended) => {
                                    this.playback = None;
                                    this.playback_starting = false;
                                    this.playback_message = Some("Playback ended".into());
                                }
                                Some(playback::Status::Failed(error)) => {
                                    this.playback = None;
                                    this.playback_starting = false;
                                    this.playback_message =
                                        Some(format!("Playback failed: {error}").into());
                                }
                                None => {}
                            }
                            cx.notify();
                            this.playback.is_some()
                        });
                        if !continue_polling {
                            return;
                        }
                    }
                }));
            }
            Err(error) => self.playback_message = Some(format!("{error:#}").into()),
        }
        cx.notify();
    }

    fn toggle_automatic_frames(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.automatic_frames {
            self.automatic_frames = false;
            self.frame_task = None;
        } else {
            self.automatic_frames = true;
            self.frame_task = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let result = this.update_in(cx, |this, window, cx| {
                        if !this.automatic_frames {
                            return false;
                        }
                        if this.loading {
                            return true;
                        }
                        let Some(offset) = this.page.as_ref().and_then(|page| page.next_offset)
                        else {
                            this.automatic_frames = false;
                            cx.notify();
                            return false;
                        };
                        this.navigation.next(offset);
                        this.load_page(window, cx);
                        true
                    });
                    match result {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(error) => {
                            log::debug!(
                                "Automatic frames ended with the preview window: {error:#}"
                            );
                            return;
                        }
                    }
                }
            }));
        }
        cx.notify();
    }

    fn selected_cell_text(&self) -> Option<&str> {
        let (row, column) = self.selected_cell?;
        self.page
            .as_ref()?
            .rows
            .get(row)?
            .get(column)
            .map(String::as_str)
    }

    fn render_sections(&self, cx: &mut Context<Self>) -> AnyElement {
        let sections = self.sections.clone();
        let selected_section = if self.navigation.bytes {
            None
        } else {
            self.navigation.section.clone()
        };
        let view = cx.entity().downgrade();
        v_flex()
            .debug_selector(|| "file-viewer-sections".into())
            .w(px(200.0))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .child(
                div().p_2().child(
                    Label::new("Contents")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                uniform_list(
                    "file-viewer-sections",
                    sections.len(),
                    move |range, _, _| {
                        range
                            .filter_map(|index| {
                                let section = sections.get(index)?.clone();
                                let view = view.clone();
                                Some(
                                    ListItem::new(ElementId::NamedInteger(
                                        "file-section".into(),
                                        index as u64,
                                    ))
                                    .toggle_state(
                                        selected_section.as_deref() == Some(section.as_str()),
                                    )
                                    .child(Label::new(section.clone()).single_line())
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(view) = view.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    view.select_section(
                                                        section.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        },
                                    ),
                                )
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.section_scroll)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn render_rows(
        &self,
        range: Range<usize>,
        view: WeakEntity<Self>,
        cx: &App,
    ) -> Vec<AnyElement> {
        let Some(page) = &self.page else {
            return Vec::new();
        };
        let columns = column_count(page);
        let generation = self.generation;
        range
            .filter_map(|row_index| {
                let row = page.rows.get(row_index)?;
                Some(
                    h_flex()
                        .h(px(32.0))
                        .w_full()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .when(row_index % 2 == 1, |row| {
                            row.bg(cx.theme().colors().surface_background)
                        })
                        .child(
                            div()
                                .w(px(52.0))
                                .flex_none()
                                .px_2()
                                .child(Label::new((row_index + 1).to_string()).color(Color::Muted)),
                        )
                        .children((0..columns).map(|column_index| {
                            let text: SharedString = row
                                .get(column_index)
                                .map(String::as_str)
                                .unwrap_or_default()
                                .to_owned()
                                .into();
                            let copy_text = text.clone();
                            let tooltip_text = text.clone();
                            let view = view.clone();
                            div()
                                .id(ElementId::NamedInteger(
                                    format!("file-cell-{row_index}").into(),
                                    column_index as u64,
                                ))
                                .debug_selector(move || {
                                    format!("file-viewer-cell-{row_index}-{column_index}")
                                })
                                .w(px(column_width(page, column_index)))
                                .h_full()
                                .flex_none()
                                .px_2()
                                .border_l_1()
                                .border_color(cx.theme().colors().border_variant)
                                .cursor_pointer()
                                .overflow_hidden()
                                .when(
                                    self.selected_cell == Some((row_index, column_index)),
                                    |cell| cell.bg(cx.theme().colors().element_selected),
                                )
                                .child(
                                    Label::new(text)
                                        .single_line()
                                        .when(page.is_hex, |label| label.buffer_font(cx)),
                                )
                                .on_click(move |_, _, cx| {
                                    if let Some(view) = view.upgrade() {
                                        view.update(cx, |view, cx| {
                                            if view.generation == generation {
                                                view.selected_cell =
                                                    Some((row_index, column_index));
                                                cx.notify();
                                            }
                                        });
                                    }
                                })
                                .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_text.to_string(),
                                    ));
                                })
                                .tooltip(Tooltip::element(move |_, _| {
                                    v_flex()
                                        .max_w(px(640.0))
                                        .gap_1()
                                        .child(Label::new(tooltip_text.clone()))
                                        .child(
                                            Label::new("Right-click to copy this cell")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .into_any_element()
                                }))
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_table(&self, page: &PreviewPage, cx: &mut Context<Self>) -> AnyElement {
        let columns = column_count(page);
        let view = cx.entity().downgrade();
        div()
            .id("file-viewer-table-scroll")
            .debug_selector(|| "file-viewer-table".into())
            .size_full()
            .overflow_x_scroll()
            .child(
                v_flex()
                    .w(px(52.0
                        + (0..columns)
                            .map(|index| column_width(page, index))
                            .sum::<f32>()))
                    .min_w_full()
                    .h_full()
                    .child(
                        h_flex()
                            .h(px(32.0))
                            .flex_none()
                            .bg(cx.theme().colors().surface_background)
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(div().w(px(52.0)).flex_none().px_2().child(Label::new("#")))
                            .children((0..columns).map(|index| {
                                div()
                                    .w(px(column_width(page, index)))
                                    .flex_none()
                                    .px_2()
                                    .border_l_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(
                                        Label::new(
                                            page.columns
                                                .get(index)
                                                .cloned()
                                                .unwrap_or_else(|| format!("Column {}", index + 1)),
                                        )
                                        .single_line(),
                                    )
                            })),
                    )
                    .child(
                        uniform_list("file-viewer-rows", page.rows.len(), move |range, _, cx| {
                            let Some(view) = view.upgrade() else {
                                return Vec::new();
                            };
                            view.read(cx).render_rows(range, view.downgrade(), cx)
                        })
                        .track_scroll(&self.row_scroll)
                        .flex_1()
                        .min_h_0(),
                    ),
            )
            .into_any_element()
    }

    fn render_metadata(&self, page: Arc<PreviewPage>) -> AnyElement {
        uniform_list(
            "file-viewer-metadata",
            page.metadata.len(),
            move |range, _, _| {
                range
                    .filter_map(|index| {
                        let (key, value) = page.metadata.get(index)?;
                        let text: SharedString = format!("{key}: {value}").into();
                        let copy_text = text.clone();
                        let tooltip_text = text.clone();
                        Some(
                            div()
                                .id(ElementId::NamedInteger(
                                    "file-metadata".into(),
                                    index as u64,
                                ))
                                .h(px(24.0))
                                .child(
                                    Label::new(text)
                                        .single_line()
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_text.to_string(),
                                    ));
                                })
                                .tooltip(Tooltip::element(move |_, _| {
                                    Label::new(tooltip_text.clone()).into_any_element()
                                })),
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.metadata_scroll)
        .h(px(96.0))
        .flex_none()
        .into_any_element()
    }
}

fn column_count(page: &PreviewPage) -> usize {
    page.columns
        .len()
        .max(page.rows.iter().map(Vec::len).max().unwrap_or_default())
}

fn column_width(page: &PreviewPage, index: usize) -> f32 {
    if page.is_hex && index == 1 {
        HEX_COLUMN_WIDTH
    } else {
        CELL_WIDTH
    }
}

fn validate_image(page: PreviewPage) -> Result<PreviewPage> {
    if let Some(bytes) = &page.image {
        anyhow::ensure!(
            bytes.len() <= MAX_IMAGE_BYTES,
            "Preview image exceeds its byte limit"
        );
        anyhow::ensure!(
            bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "Invalid preview image"
        );
        let dimensions = bytes
            .get(16..24)
            .context("Missing preview image dimensions")?;
        let width = u32::from_be_bytes(
            dimensions
                .get(..4)
                .context("Missing image width")?
                .try_into()?,
        );
        let height = u32::from_be_bytes(
            dimensions
                .get(4..)
                .context("Missing image height")?
                .try_into()?,
        );
        anyhow::ensure!(
            width > 0 && height > 0 && width <= 1024 && height <= 1024,
            "Invalid preview image dimensions"
        );
        anyhow::ensure!(
            u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
            "Preview image exceeds its pixel limit"
        );
    }
    Ok(page)
}

fn page_as_tsv(page: &PreviewPage) -> String {
    let mut text = String::new();
    let mut append_row = |row: &[String]| {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                text.push('\t');
            }
            if value.contains(['\t', '\n', '\r', '"']) {
                text.push('"');
                text.push_str(&value.replace('"', "\"\""));
                text.push('"');
            } else {
                text.push_str(value);
            }
        }
        text.push('\n');
    };
    if !page.columns.is_empty() {
        append_row(&page.columns);
    }
    for row in &page.rows {
        append_row(row);
    }
    text
}

impl Focusable for FileView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for FileView {}

impl Item for FileView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, callback: &mut dyn FnMut(ItemEvent)) {
        callback(*event);
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .current_project_path(cx)
            .path
            .as_std_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "File preview".into())
            .into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            format!(
                "{} · Read-only",
                self.item.read(cx).absolute_path(cx)?.display()
            )
            .into(),
        )
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.item.read(cx).absolute_path(cx)?;
        FileIcons::get_icon(&path, cx).map(Icon::from_path)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        callback(self.item.entity_id(), self.item.read(cx));
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn capability(&self, _: &App) -> Capability {
        Capability::ReadOnly
    }

    fn can_save(&self, _: &App) -> bool {
        false
    }

    fn can_save_as(&self, _: &App) -> bool {
        false
    }

    fn can_split(&self) -> bool {
        true
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        let item = self.item.read(cx);
        item.entry_id.is_some_and(|entry_id| {
            item.project
                .upgrade()
                .is_some_and(|project| project.read(cx).path_for_entry(entry_id, cx).is_none())
        })
    }

    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let item = self.item.clone();
        let navigation = self.navigation.clone();
        Task::ready(Some(
            cx.new(|cx| Self::with_navigation(item, navigation, window, cx)),
        ))
    }
}

impl ProjectItem for FileView {
    type Item = FileItem;

    fn for_project_item(
        _: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<FileItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, window, cx)
    }
}

impl Render for FileView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page.clone();
        let next_available =
            !self.loading && page.as_ref().is_some_and(|page| page.next_offset.is_some());
        let selected_cell = self.selected_cell_text().map(str::to_owned);
        let media_kind = self.media_kind(cx);
        let media_controls = media_kind.is_some() && !self.navigation.bytes;

        v_flex()
            .debug_selector(|| "file-viewer-root".into())
            .key_context("FileViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .h(px(40.0))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Icon::new(IconName::Lock).color(Color::Muted))
                    .child(
                        div()
                            .debug_selector(|| "file-viewer-read-only".into())
                            .child(
                                Label::new("Read-only")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("file-viewer-previous", "Previous")
                            .disabled(self.loading || self.navigation.previous_offsets.is_empty())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.previous_page(window, cx)),
                            ),
                    )
                    .child(
                        Label::new(format!(
                            "Page {}",
                            self.navigation.page_number.saturating_add(1)
                        ))
                        .size(LabelSize::Small),
                    )
                    .child(
                        Button::new("file-viewer-next", "Next")
                            .disabled(!next_available)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.next_page(window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "file-viewer-bytes-button".into())
                            .child(
                                Button::new(
                                    "file-viewer-bytes",
                                    if self.navigation.bytes {
                                        "Preview"
                                    } else {
                                        "Hex"
                                    },
                                )
                                .toggle_state(self.navigation.bytes)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.toggle_bytes(window, cx)
                                    }),
                                ),
                            ),
                    )
                    .child(
                        Button::new("file-viewer-refresh", "Refresh")
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    )
                    .child(
                        Button::new(
                            "file-viewer-copy-page",
                            if self.copied_page {
                                "Page copied"
                            } else {
                                "Copy page"
                            },
                        )
                        .disabled(page.is_none())
                        .on_click(cx.listener(|this, _, _, cx| this.copy_page(cx))),
                    ),
            )
            .when(media_controls, |view| {
                view.child(
                    h_flex()
                        .h(px(36.0))
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Button::new(
                                "file-viewer-play",
                                if self.playback.is_none() {
                                    if media_kind == Some(file_preview::FileKind::Video) {
                                        "Play audio"
                                    } else {
                                        "Play"
                                    }
                                } else if self.playback_paused {
                                    "Resume"
                                } else {
                                    "Pause"
                                },
                            )
                            .disabled(self.playback_starting || !self.item.read(cx).local)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_playback(cx))),
                        )
                        .child(
                            Button::new("file-viewer-stop", "Stop")
                                .disabled(self.playback.is_none() && !self.automatic_frames)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.stop_media();
                                    cx.notify();
                                })),
                        )
                        .when(
                            media_kind == Some(file_preview::FileKind::Video),
                            |controls| {
                                controls.child(
                                    Button::new("file-viewer-auto-frames", "Auto frames")
                                        .toggle_state(self.automatic_frames)
                                        .disabled(!self.automatic_frames && !next_available)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.toggle_automatic_frames(window, cx)
                                        })),
                                )
                            },
                        )
                        .when_some(self.playback_message.clone(), |controls, message| {
                            controls.child(
                                Label::new(message)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when(!self.sections.is_empty(), |content| {
                        content.child(self.render_sections(cx))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .p_3()
                            .gap_2()
                            .when(self.loading, |content| {
                                content.child(Label::new("Loading preview…").color(Color::Muted))
                            })
                            .when_some(self.error.clone(), |content, error| {
                                content.child(
                                    div()
                                        .debug_selector(|| "file-viewer-error".into())
                                        .child(Label::new(error).color(Color::Error)),
                                )
                            })
                            .when_some(page, |content, page| {
                                content
                                    .when(!page.title.is_empty(), |content| {
                                        content.child(Label::new(page.title.clone()))
                                    })
                                    .when(!page.metadata.is_empty(), |content| {
                                        content.child(self.render_metadata(page.clone()))
                                    })
                                    .when_some(page.note.clone(), |content, note| {
                                        content.child(
                                            Label::new(note)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    })
                                    .when_some(self.image.clone(), |content, image| {
                                        content.child(
                                            div()
                                                .debug_selector(|| "file-viewer-image".into())
                                                .flex_1()
                                                .min_h_0()
                                                .child(img(image).size_full()),
                                        )
                                    })
                                    .when(!page.rows.is_empty(), |content| {
                                        content.child(
                                            div()
                                                .flex_1()
                                                .min_h_0()
                                                .child(self.render_table(&page, cx)),
                                        )
                                    })
                                    .when(page.rows.is_empty() && self.image.is_none(), |content| {
                                        content.child(
                                            Label::new("No rows in this section.")
                                                .color(Color::Muted),
                                        )
                                    })
                                    .child(
                                        Label::new(format!(
                                            "{} rows on this page",
                                            page.rows.len()
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    )
                            })
                            .when_some(selected_cell, |content, text| {
                                let copy_text = text.clone();
                                content.child(
                                    v_flex()
                                        .flex_none()
                                        .gap_1()
                                        .border_t_1()
                                        .border_color(cx.theme().colors().border)
                                        .pt_2()
                                        .child(
                                            h_flex()
                                                .justify_between()
                                                .child(
                                                    Label::new("Selected cell")
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Button::new(
                                                        "file-viewer-copy-cell",
                                                        "Copy cell",
                                                    )
                                                    .on_click(move |_, _, cx| {
                                                        cx.write_to_clipboard(
                                                            ClipboardItem::new_string(
                                                                copy_text.clone(),
                                                            ),
                                                        );
                                                    }),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("file-viewer-cell-detail")
                                                .max_h(px(100.0))
                                                .overflow_y_scroll()
                                                .font_buffer(cx)
                                                .child(text),
                                        ),
                                )
                            }),
                    ),
            )
    }
}

impl SerializableItem for FileView {
    fn serialized_item_kind() -> &'static str {
        "FileView"
    }

    fn deserialize(
        project: Entity<Project>,
        _: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let database = FileViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let path = database
                .get_file_path(item_id, workspace_id)?
                .context("No preview file path found")?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(path, false, cx)
                })
                .await?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
            let project_path = ProjectPath { worktree_id, path };
            let task = cx.update(|_, cx| {
                <FileItem as project::ProjectItem>::try_open_async(&project, &project_path, cx)
            })?;
            let item = task
                .await?
                .context("This file does not require a binary preview")?;
            cx.update(|window, cx| cx.new(|cx| Self::new(item, window, cx)))
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let database = FileViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "file_viewers", &database, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let path = self.item.read(cx).absolute_path(cx)?;
        let database = FileViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            database.save_file_path(item_id, workspace_id, path).await
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<FileView>(cx);
    workspace::register_serializable_item::<FileView>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct FileViewerDb(ThreadSafeConnection);

    impl Domain for FileViewerDb {
        const NAME: &'static str = stringify!(FileViewerDb);
        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE file_viewers (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                file_path BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(FileViewerDb, [WorkspaceDb]);

    impl FileViewerDb {
        query! {
            pub async fn save_file_path(item_id: ItemId, workspace_id: WorkspaceId, file_path: PathBuf) -> Result<()> {
                INSERT OR REPLACE INTO file_viewers(item_id, workspace_id, file_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_file_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT file_path FROM file_viewers WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::Path};

    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::ProjectItem as _;
    use settings::SettingsStore;
    use util::rel_path::rel_path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[test]
    fn pagination_uses_backend_offsets_and_resets_for_sections() {
        let mut navigation = PageNavigation::default();
        navigation.next(120);
        navigation.next(543);
        assert_eq!(navigation.request().offset, 543);
        assert!(navigation.previous());
        assert_eq!(navigation.request().offset, 120);
        navigation.select_section(Some("other_table".into()));
        assert_eq!(navigation.request().offset, 0);
        assert!(navigation.previous_offsets.is_empty());
        assert!(!navigation.previous());
        assert_eq!(navigation.request().section.as_deref(), Some("other_table"));
    }

    #[test]
    fn copy_page_preserves_multiline_and_tabbed_cells() {
        let page = PreviewPage {
            columns: vec!["name".into(), "value".into()],
            rows: vec![vec!["first\tsecond".into(), "quoted \"text\"\nnext".into()]],
            ..Default::default()
        };
        assert_eq!(
            page_as_tsv(&page),
            "name\tvalue\n\"first\tsecond\"\t\"quoted \"\"text\"\"\nnext\"\n"
        );
    }

    #[test]
    fn navigation_history_has_a_fixed_memory_budget() {
        let mut navigation = PageNavigation::default();
        for offset in 1..=MAX_PAGE_HISTORY as u64 + 10 {
            navigation.next(offset);
        }
        assert_eq!(navigation.previous_offsets.len(), MAX_PAGE_HISTORY);
        assert_eq!(navigation.page_number, MAX_PAGE_HISTORY as u64 + 10);
        assert!(navigation.previous());
        assert_eq!(navigation.page_number, MAX_PAGE_HISTORY as u64 + 9);
    }

    #[test]
    fn bytes_can_return_to_default_preview_or_the_selected_section() {
        let mut navigation = PageNavigation::default();
        navigation.toggle_bytes();
        assert!(navigation.bytes);
        assert!(navigation.section.is_none());
        navigation.next(128);
        navigation.toggle_bytes();
        assert!(!navigation.bytes);
        assert!(navigation.section.is_none());
        assert_eq!(navigation.offset, 0);
        navigation.select_section(Some("Players".into()));
        navigation.toggle_bytes();
        navigation.toggle_bytes();
        assert_eq!(navigation.section.as_deref(), Some("Players"));
        assert!(navigation.previous_offsets.is_empty());
    }

    #[test]
    fn bytes_is_a_valid_content_section_and_has_an_independent_raw_mode() {
        let mut navigation = PageNavigation::default();
        navigation.select_section(Some("Bytes".into()));
        assert!(!navigation.bytes);
        assert_eq!(navigation.request().section.as_deref(), Some("Bytes"));
        navigation.toggle_bytes();
        assert!(navigation.bytes);
        assert_eq!(navigation.section.as_deref(), Some("Bytes"));
        navigation.next(3200);
        navigation.toggle_bytes();
        assert!(!navigation.bytes);
        assert_eq!(navigation.section.as_deref(), Some("Bytes"));
        assert_eq!(navigation.offset, 0);
        navigation.toggle_bytes();
        navigation.select_section(Some("Bytes".into()));
        assert!(!navigation.bytes);
    }

    #[test]
    fn compressed_image_cannot_exceed_decoded_pixel_budget() {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&50000_u32.to_be_bytes());
        bytes.extend_from_slice(&50000_u32.to_be_bytes());
        let page = PreviewPage {
            image: Some(bytes),
            ..Default::default()
        };
        assert!(validate_image(page).is_err());
    }

    #[gpui::test]
    async fn recognized_formats_route_to_read_only_items(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({
                "data.sqlite": "not opened as an editor buffer",
                "book.xlsx": "",
                "page.pdf": "",
                "image.tga": "",
                "model.fbx": "",
                "plain.rs": "fn main() {}",
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        for name in [
            "data.sqlite",
            "book.xlsx",
            "page.pdf",
            "image.tga",
            "model.fbx",
        ] {
            let path = ProjectPath {
                worktree_id,
                path: rel_path(name).into(),
            };
            let task = cx
                .update(|cx| FileItem::try_open(&project, &path, cx))
                .expect("recognized format should claim the path");
            let item = task
                .await
                .expect("creating a preview model should not read its contents");
            let view = cx.new(|cx| FileView::unloaded(item, cx));
            view.read_with(cx, |view, cx| {
                assert_eq!(view.capability(cx), Capability::ReadOnly);
                assert!(!view.can_save(cx));
                assert!(!view.can_save_as(cx));
                assert!(!view.is_dirty(cx));
                assert_eq!(view.active_project_path(cx), Some(path.clone()));
            });
        }
        let path = ProjectPath {
            worktree_id,
            path: rel_path("plain.rs").into(),
        };
        assert!(
            cx.update(|cx| FileItem::try_open(&project, &path, cx))
                .is_none()
        );
    }

    #[gpui::test]
    async fn a_real_bytes_section_opens_content_and_can_toggle_to_hex(cx: &mut TestAppContext) {
        init_test(cx);
        let directory = tempfile::tempdir().expect("test directory");
        let contents = serde_json::json!({
            "asset":{"version":"2.0"},
            "Bytes":["real section content"],
            "padding":"x".repeat(5000),
        })
        .to_string();
        std::fs::write(directory.path().join("model.gltf"), &contents).expect("test model");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(directory.path(), serde_json::json!({"model.gltf":contents}))
            .await;
        let project = Project::test(fs, [directory.path()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("model.gltf").into(),
        };
        let item = cx
            .update(|cx| FileItem::try_open(&project, &path, cx))
            .expect("model route")
            .await
            .expect("model item");
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = FileView::unloaded(item, cx);
            view.select_section("Bytes".into(), window, cx);
            view
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.navigation.bytes);
            assert!(view.error.is_none());
            let page = view.page.as_ref().expect("actual named section");
            assert_eq!(page.title, "Bytes");
            assert_eq!(
                page.rows
                    .first()
                    .and_then(|row| row.get(1))
                    .map(String::as_str),
                Some("\"real section content\"")
            );
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.toggle_bytes(window, cx)));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.navigation.bytes);
            let page = view.page.as_ref().expect("raw byte page");
            assert_eq!(page.columns.first().map(String::as_str), Some("Offset"));
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.next_page(window, cx)));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.navigation.bytes);
            assert_eq!(view.navigation.offset, 3200);
            assert_eq!(view.navigation.page_number, 1);
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.refresh(window, cx)));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.navigation.bytes);
            assert_eq!(view.navigation.offset, 0);
            assert_eq!(view.navigation.page_number, 0);
            assert!(view.navigation.previous_offsets.is_empty());
            let page = view.page.as_ref().expect("refreshed raw byte page");
            assert_eq!(page.columns.first().map(String::as_str), Some("Offset"));
            assert_eq!(
                page.rows
                    .first()
                    .and_then(|row| row.first())
                    .map(String::as_str),
                Some("0000000000000000")
            );
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.toggle_bytes(window, cx)));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.navigation.bytes);
            assert_eq!(view.page.as_ref().expect("content restored").title, "Bytes");
        });
    }

    #[gpui::test]
    async fn text_encodings_and_new_files_remain_available_to_the_editor(cx: &mut TestAppContext) {
        init_test(cx);
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree(
                "/project",
                serde_json::json!({
                    "plain.rs": "fn main() {}",
                    "plain.unknown": "ordinary text",
                    "README": "ordinary text",
                    "utf16-le.unknown": "",
                    "utf16-be.unknown": "",
                    "utf16-bom.unknown": "",
                    "gbk.unknown": "",
                    "folder": {},
                }),
            )
            .await;
        let little_endian = "ordinary text\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let big_endian = "ordinary text\n"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let with_bom = [vec![0xff, 0xfe], little_endian.clone()].concat();
        for (name, contents) in [
            ("utf16-le.unknown", little_endian),
            ("utf16-be.unknown", big_endian),
            ("utf16-bom.unknown", with_bom),
            ("gbk.unknown", vec![0xd6, 0xd0, 0xce, 0xc4]),
        ] {
            fs::Fs::write(
                filesystem.as_ref(),
                &Path::new("/project").join(name),
                &contents,
            )
            .await
            .expect("text fixture");
        }
        let project = Project::test(filesystem, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        for name in [
            "plain.rs",
            "plain.unknown",
            "README",
            "utf16-le.unknown",
            "utf16-be.unknown",
            "utf16-bom.unknown",
            "gbk.unknown",
            "folder",
            "new.rs",
        ] {
            let path = ProjectPath {
                worktree_id,
                path: rel_path(name).into(),
            };
            assert!(
                cx.update(|cx| FileItem::try_open_async(&project, &path, cx))
                    .await
                    .expect("file probe")
                    .is_none(),
                "{name} should remain available to the editor"
            );
        }
    }

    #[gpui::test]
    async fn restoring_an_unknown_binary_file_opens_hex(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("restored.unknown");
        let contents = b"binary\0file\0contents";
        std::fs::write(&path, contents).expect("binary fixture");
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree(
                directory.path(),
                serde_json::json!({"restored.unknown":"binary\0file\0contents"}),
            )
            .await;
        let project = Project::test(filesystem, [directory.path()], cx).await;
        let workspace_id = cx
            .read(workspace::WorkspaceDb::global)
            .next_id()
            .await
            .expect("reserved workspace");
        cx.read(FileViewerDb::global)
            .save_file_path(1, workspace_id, path.clone())
            .await
            .expect("persisted binary path");
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path("restored.unknown").into(),
        };
        let item = cx.update(|cx| FileItem::new(&project, &project_path, cx));
        let (_, window_context) = cx.add_window_view(|_, cx| FileView::unloaded(item, cx));
        let restored = window_context
            .update(|window, cx| {
                FileView::deserialize(
                    project.clone(),
                    WeakEntity::new_invalid(),
                    workspace_id,
                    1,
                    window,
                    cx,
                )
            })
            .await
            .expect("restored binary viewer");
        window_context.run_until_parked();
        restored.read_with(window_context, |view, cx| {
            assert!(view.navigation.bytes);
            assert!(view.error.is_none());
            assert_eq!(view.capability(cx), Capability::ReadOnly);
            assert_eq!(view.active_project_path(cx), Some(project_path));
            let page = view.page.as_ref().expect("restored hex page");
            assert!(page.is_hex);
            assert_eq!(
                page.rows[0][1],
                "62 69 6E 61 72 79 00 66 69 6C 65 00 63 6F 6E 74 "
            );
        });
        assert_eq!(std::fs::read(path).expect("unchanged binary"), contents);
    }

    #[gpui::test]
    async fn unsupported_files_open_as_hex_and_keep_paging_after_refresh(cx: &mut TestAppContext) {
        init_test(cx);
        let directory = tempfile::tempdir().expect("test directory");
        let mut contents = b"unsupported file contents".to_vec();
        contents.resize(6500, 0);
        for name in ["broken.sqlite", "asset.unknown", "no-extension"] {
            std::fs::write(directory.path().join(name), &contents).expect("test binary");
        }
        let text = String::from_utf8(contents.clone()).expect("ASCII fixture");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            directory.path(),
            serde_json::json!({
                "broken.sqlite": text,
                "asset.unknown": text,
                "no-extension": text,
            }),
        )
        .await;
        let project = Project::test(fs, [directory.path()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        for name in ["broken.sqlite", "asset.unknown", "no-extension"] {
            let path = ProjectPath {
                worktree_id,
                path: rel_path(name).into(),
            };
            let item = cx
                .update(|cx| FileItem::try_open_async(&project, &path, cx))
                .await
                .expect("file route")
                .expect("binary viewer item");
            let (view, window_context) =
                cx.add_window_view(|window, cx| FileView::new(item, window, cx));
            window_context.run_until_parked();
            view.read_with(window_context, |view, cx| {
                assert!(view.navigation.bytes);
                assert!(view.error.is_none());
                assert_eq!(view.capability(cx), Capability::ReadOnly);
                assert!(!view.can_save(cx));
                let page = view.page.as_ref().expect("automatic hex page");
                assert!(page.is_hex);
                assert_eq!(page.title, "Hex");
                assert_eq!(page.rows.len(), file_preview::PAGE_ROWS);
                assert!(page.note.is_some());
                assert_eq!(page.next_offset, Some(3200));
            });
            assert!(!project.read_with(window_context, |project, cx| {
                project.has_open_buffer(path.clone(), cx)
            }));
            window_context.simulate_resize(gpui::size(px(1200.0), px(800.0)));
            window_context.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let hex_cell = window_context
                .debug_bounds("file-viewer-cell-0-1")
                .expect("visible hex column");
            assert_eq!(hex_cell.size.width, px(HEX_COLUMN_WIDTH));
            window_context.simulate_mouse_down(
                hex_cell.center(),
                MouseButton::Right,
                gpui::Modifiers::default(),
            );
            assert_eq!(
                window_context.read(|cx| cx.read_from_clipboard().expect("hex clipboard").text()),
                Some("75 6E 73 75 70 70 6F 72 74 65 64 20 66 69 6C 65 ".into())
            );
            window_context
                .update(|window, cx| view.update(cx, |view, cx| view.next_page(window, cx)));
            window_context.run_until_parked();
            view.read_with(window_context, |view, _| {
                assert!(view.navigation.bytes);
                assert_eq!(view.navigation.offset, 3200);
                assert_eq!(view.navigation.page_number, 1);
                let page = view.page.as_ref().expect("second hex page");
                assert_eq!(page.rows[0][0], "0000000000000C80");
                assert_eq!(page.next_offset, Some(6400));
            });
            window_context
                .update(|window, cx| view.update(cx, |view, cx| view.refresh(window, cx)));
            window_context.run_until_parked();
            view.read_with(window_context, |view, _| {
                assert!(view.navigation.bytes);
                assert_eq!(view.navigation.offset, 0);
                assert_eq!(view.navigation.page_number, 0);
                assert!(view.navigation.previous_offsets.is_empty());
                assert_eq!(
                    view.page.as_ref().expect("refreshed hex page").rows[0][0],
                    "0000000000000000"
                );
            });
            assert_eq!(
                std::fs::read(directory.path().join(name)).expect("unchanged binary"),
                contents
            );
        }
    }

    #[gpui::test]
    async fn obsolete_page_results_cannot_replace_current_content(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "data.db": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("data.db").into(),
        };
        let item = cx
            .update(|cx| FileItem::try_open(&project, &path, cx))
            .expect("database route")
            .await
            .expect("preview model");
        let view = cx.new(|cx| FileView::unloaded(item, cx));
        view.update(cx, |view, cx| {
            view.generation = 2;
            view.loading = true;
            view.apply_result(
                1,
                Ok(PreviewPage {
                    title: "obsolete".into(),
                    ..Default::default()
                }),
                cx,
            );
            assert!(view.loading);
            assert!(view.page.is_none());
            view.apply_result(
                2,
                Ok(PreviewPage {
                    title: "current".into(),
                    ..Default::default()
                }),
                cx,
            );
            view.apply_result(1, Err(anyhow::anyhow!("obsolete error")), cx);
            assert!(!view.loading);
            assert_eq!(
                view.page.as_ref().map(|page| page.title.as_str()),
                Some("current")
            );
            assert!(view.error.is_none());
        });
    }

    #[gpui::test]
    async fn releasing_split_images_preserves_then_releases_the_shared_cache(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "image.png": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("image.png").into(),
        };
        let item = cx
            .update(|cx| FileItem::try_open(&project, &path, cx))
            .expect("image route")
            .await
            .expect("image model");
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("test PNG");
        let bytes = encoded.into_inner();
        let cx = cx.add_empty_window();
        let original = cx.update(|window, cx| {
            cx.new(|cx| {
                let mut view = FileView::new(item.clone(), window, cx);
                view.cancel_read();
                view.apply_result(
                    view.generation,
                    Ok(PreviewPage {
                        image: Some(bytes.clone()),
                        ..Default::default()
                    }),
                    cx,
                );
                view
            })
        });
        let split = cx.update(|window, cx| {
            cx.new(|cx| {
                let mut view = FileView::new(item, window, cx);
                view.cancel_read();
                view.apply_result(
                    view.generation,
                    Ok(PreviewPage {
                        image: Some(bytes),
                        ..Default::default()
                    }),
                    cx,
                );
                view
            })
        });
        let image = split
            .read_with(cx, |view, _| view.image.clone())
            .expect("split PNG");
        struct ImageFixture(Arc<gpui::Image>);
        impl Render for ImageFixture {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                img(self.0.clone()).size_full()
            }
        }
        let image_view = cx.new(|_| ImageFixture(image.clone()));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.draw(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(2.0), px(2.0)),
                |_, _| image_view.clone().into_any_element(),
            );
        };
        draw(cx);
        cx.run_until_parked();
        draw(cx);
        let rendered = cx
            .update(|window, cx| image.clone().get_render_image(window, cx))
            .expect("decoded image");
        assert!(cx.update(|window, _| window.has_image_atlas_entry(&rendered)));
        drop(original);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(cx.update(|window, _| window.has_image_atlas_entry(&rendered)));
        assert!(cx.read(|cx| image.is_asset_cached(cx)));
        drop(split);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(!cx.update(|window, _| window.has_image_atlas_entry(&rendered)));
        assert!(!cx.read(|cx| image.is_asset_cached(cx)));
    }

    #[gpui::test]
    async fn closing_the_window_before_the_view_releases_its_cached_image(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "image.png": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("image.png").into(),
        };
        let item = cx
            .update(|cx| FileItem::try_open(&project, &path, cx))
            .expect("image route")
            .await
            .expect("image model");
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("test PNG");
        let bytes = encoded.into_inner();
        let (view, image) = {
            let window_context = cx.add_empty_window();
            let view = window_context.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = FileView::new(item, window, cx);
                    view.cancel_read();
                    view.apply_result(
                        view.generation,
                        Ok(PreviewPage {
                            image: Some(bytes),
                            ..Default::default()
                        }),
                        cx,
                    );
                    view
                })
            });
            let image = view
                .read_with(window_context, |view, _| view.image.clone())
                .expect("preview PNG");
            window_context.update(|window, cx| {
                image.clone().get_render_image(window, cx);
            });
            window_context.run_until_parked();
            assert!(window_context.read(|cx| image.is_asset_cached(cx)));
            window_context.update(|window, _| window.remove_window());
            (view, image)
        };
        drop(view);
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(!cx.read(|cx| image.is_asset_cached(cx)));
        assert!(cx.read(|cx| cx.global::<ImageOwners>().0.is_empty()));
    }

    #[gpui::test]
    async fn table_cells_errors_and_images_render_in_the_read_only_view(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "data.db": "" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("test worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("data.db").into(),
        };
        let item = cx
            .update(|cx| FileItem::try_open(&project, &path, cx))
            .expect("database route")
            .await
            .expect("database model");
        let page = PreviewPage {
            title: "Players".into(),
            sections: vec!["Schema".into(), "Players".into()],
            columns: vec!["id".into(), "name".into()],
            rows: (0..file_preview::PAGE_ROWS)
                .map(|index| vec![index.to_string(), format!("player-{index}")])
                .collect(),
            metadata: (0..500)
                .map(|index| (format!("Property {index}"), "value".into()))
                .collect(),
            next_offset: Some(200),
            ..Default::default()
        };
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = FileView::new(item, window, cx);
            view.cancel_read();
            view.navigation.select_section(Some("Players".into()));
            view.apply_result(view.generation, Ok(page), cx);
            view
        });
        cx.simulate_resize(gpui::size(px(1200.0), px(800.0)));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        };
        draw(cx);
        let read_only = cx
            .debug_bounds("file-viewer-read-only")
            .expect("visible read-only label");
        assert!(read_only.size.width > px(0.0));
        let table = cx.debug_bounds("file-viewer-table").expect("visible table");
        assert!(table.size.width > px(400.0));
        assert!(table.size.height > px(100.0));
        assert!(cx.debug_bounds("file-viewer-sections").is_some());
        let cell = cx
            .debug_bounds("file-viewer-cell-0-1")
            .expect("visible first name cell");
        cx.simulate_click(cell.center(), gpui::Modifiers::default());
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_cell),
            Some((0, 1))
        );
        cx.simulate_mouse_down(
            cell.center(),
            MouseButton::Right,
            gpui::Modifiers::default(),
        );
        assert_eq!(
            cx.read(|cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("player-0".into())
        );
        assert!(!cx.read(|cx| project.read(cx).has_open_buffer(path.clone(), cx)));
        view.update(cx, |view, cx| {
            view.page = None;
            view.error = Some("Malformed database header".into());
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("file-viewer-error").is_some());
        assert!(cx.debug_bounds("file-viewer-bytes-button").is_some());
        assert!(cx.debug_bounds("file-viewer-table").is_none());
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("test PNG");
        view.update(cx, |view, cx| {
            view.apply_result(
                view.generation,
                Ok(PreviewPage {
                    image: Some(encoded.into_inner()),
                    ..Default::default()
                }),
                cx,
            )
        });
        draw(cx);
        cx.run_until_parked();
        draw(cx);
        let image = cx
            .debug_bounds("file-viewer-image")
            .expect("visible PNG preview");
        assert!(image.size.width > px(0.0));
        assert!(image.size.height > px(0.0));
        assert!(cx.debug_bounds("file-viewer-error").is_none());
        assert_eq!(
            view.read_with(cx, |view, cx| view.capability(cx)),
            Capability::ReadOnly
        );
    }
}
