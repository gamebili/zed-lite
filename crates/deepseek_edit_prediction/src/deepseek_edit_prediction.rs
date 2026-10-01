mod bundled_key;

use std::{
    ops::Range,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger,
};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::{HttpClient, HttpRequestExt as _};
use icons::IconName;
use language::{
    Anchor, Buffer, BufferSnapshot, EditPreview, Point, language_settings::all_language_settings,
};
use serde::{Deserialize, Serialize};
use text::ToOffset as _;
use util::ResultExt as _;

pub use bundled_key::{BundledCredentials, KEY_FILE_NAME};

const MAX_PREFIX_LINES: u32 = 120;
const MAX_SUFFIX_LINES: u32 = 40;
const MAX_PREFIX_BYTES: usize = 12_000;
const MAX_SUFFIX_BYTES: usize = 4_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CURSOR_MARKER: &str = "<|CURSOR|>";

const SYSTEM_PROMPT: &str = "You are a code completion engine embedded in a text editor. \
The user message contains an excerpt of a file with the marker <|CURSOR|> at the caret. \
Reply with ONLY the text that should be inserted at <|CURSOR|>: no explanations, no markdown \
code fences, and never repeat text that already appears before or after the marker. \
Prefer finishing the current line or statement; never write more than a few lines. \
If nothing sensible should be inserted, reply with an empty message.";

static CREDENTIALS: LazyLock<Result<Arc<BundledCredentials>, SharedString>> =
    LazyLock::new(|| match bundled_key::load() {
        Ok(credentials) => {
            log::info!(
                "DeepSeek edit predictions: using endpoint {}",
                credentials.url
            );
            Ok(Arc::new(credentials))
        }
        Err(error) => {
            let message = format!("{error:#}");
            log::error!("DeepSeek edit predictions unavailable: {message}");
            Err(message.into())
        }
    });

/// Decrypts the bundled `key.enc` on first use; later calls reuse the result.
pub fn credentials() -> Result<Arc<BundledCredentials>, SharedString> {
    CREDENTIALS.clone()
}

/// The most recent failure, shown by the status bar button so users see why nothing is predicted.
#[derive(Default)]
pub struct DeepSeekStatus {
    pub last_error: Option<SharedString>,
}

struct GlobalDeepSeekStatus(Entity<DeepSeekStatus>);

impl Global for GlobalDeepSeekStatus {}

pub fn status(cx: &mut App) -> Entity<DeepSeekStatus> {
    if let Some(global) = cx.try_global::<GlobalDeepSeekStatus>() {
        return global.0.clone();
    }
    let status = cx.new(|_| DeepSeekStatus::default());
    cx.set_global(GlobalDeepSeekStatus(status.clone()));
    status
}

fn set_last_error(status: &Entity<DeepSeekStatus>, error: Option<SharedString>, cx: &mut App) {
    status.update(cx, |status, cx| {
        if status.last_error != error {
            status.last_error = error;
            cx.notify();
        }
    });
}

#[derive(Clone)]
struct CurrentCompletion {
    snapshot: BufferSnapshot,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    edit_preview: EditPreview,
}

impl CurrentCompletion {
    fn interpolate(&self, new_snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        edit_prediction_types::interpolate_edits(&self.snapshot, new_snapshot, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

pub struct DeepSeekEditPredictionDelegate {
    http_client: Arc<dyn HttpClient>,
    status: Entity<DeepSeekStatus>,
    pending_request: Option<Task<()>>,
    current_completion: Option<CurrentCompletion>,
}

impl DeepSeekEditPredictionDelegate {
    pub fn new(http_client: Arc<dyn HttpClient>, cx: &mut App) -> Self {
        Self {
            http_client,
            status: status(cx),
            pending_request: None,
            current_completion: None,
        }
    }
}

impl EditPredictionDelegate for DeepSeekEditPredictionDelegate {
    fn name() -> &'static str {
        "deepseek"
    }

    fn display_name() -> &'static str {
        "DeepSeek"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::AiDeepSeek)
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, _cx: &App) -> bool {
        credentials().is_ok()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        _trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        let credentials = match credentials() {
            Ok(credentials) => credentials,
            Err(error) => {
                set_last_error(&self.status, Some(error), cx);
                return;
            }
        };

        let snapshot = buffer.read(cx).snapshot();
        if let Some(current_completion) = self.current_completion.as_ref()
            && current_completion.interpolate(&snapshot).is_some()
        {
            return;
        }

        let settings = &all_language_settings(None, cx).edit_predictions.deepseek;
        let request = RequestOptions {
            model: settings.model.clone(),
            max_tokens: settings.max_tokens,
        };
        let file_path = buffer
            .read(cx)
            .file()
            .map(|file| file.full_path(cx).to_string_lossy().into_owned());
        let language_name = snapshot
            .language()
            .map(|language| language.name().to_string());
        let http_client = self.http_client.clone();
        let status = self.status.clone();

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                cx.background_executor().timer(debounce_duration).await;
            }

            let excerpt = Excerpt::around(&snapshot, cursor_position.to_offset(&snapshot));
            let user_message = excerpt.prompt(file_path.as_deref(), language_name.as_deref());
            let started_at = Instant::now();
            let response =
                fetch_completion(http_client, &credentials, &request, user_message).await;

            let completion = match response {
                Ok(raw) => {
                    log::debug!(
                        "DeepSeek: completion received in {:.2}s",
                        started_at.elapsed().as_secs_f64()
                    );
                    cx.update(|cx| set_last_error(&status, None, cx));
                    clean_completion(&raw, &excerpt)
                }
                Err(error) => {
                    log::error!("DeepSeek: failed to fetch completion: {error:#}");
                    cx.update(|cx| set_last_error(&status, Some(format!("{error:#}").into()), cx));
                    None
                }
            };

            let Some(completion) = completion else {
                this.update(cx, |this, cx| {
                    this.pending_request = None;
                    cx.notify();
                })
                .log_err();
                return;
            };

            let edits: Arc<[(Range<Anchor>, Arc<str>)]> =
                vec![(cursor_position..cursor_position, completion.into())].into();
            let edit_preview = buffer
                .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                .await;

            this.update(cx, |this, cx| {
                this.current_completion = Some(CurrentCompletion {
                    snapshot,
                    edits,
                    edit_preview,
                });
                this.pending_request = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let current_completion = self.current_completion.as_ref()?;
        let edits = current_completion.interpolate(&buffer.read(cx).snapshot())?;
        Some(EditPrediction::Local {
            id: None,
            edits,
            cursor_position: None,
            edit_preview: Some(current_completion.edit_preview.clone()),
        })
    }
}

struct RequestOptions {
    model: String,
    max_tokens: u32,
}

/// The text around the cursor that is sent to the model, cut at line boundaries.
#[derive(Debug, PartialEq)]
struct Excerpt {
    prefix: String,
    suffix: String,
}

impl Excerpt {
    fn around(snapshot: &BufferSnapshot, cursor_offset: usize) -> Self {
        let cursor_point = snapshot.offset_to_point(cursor_offset);
        let start_row = cursor_point.row.saturating_sub(MAX_PREFIX_LINES);
        let end_row = cursor_point
            .row
            .saturating_add(MAX_SUFFIX_LINES)
            .min(snapshot.max_point().row);
        let start = snapshot.point_to_offset(Point::new(start_row, 0));
        let end = snapshot.point_to_offset(Point::new(end_row, snapshot.line_len(end_row)));
        let prefix: String = snapshot.text_for_range(start..cursor_offset).collect();
        let suffix: String = snapshot.text_for_range(cursor_offset..end).collect();
        Self::from_parts(prefix, suffix)
    }

    fn from_parts(prefix: String, suffix: String) -> Self {
        Self {
            prefix: keep_tail(prefix, MAX_PREFIX_BYTES),
            suffix: keep_head(suffix, MAX_SUFFIX_BYTES),
        }
    }

    fn prompt(&self, file_path: Option<&str>, language_name: Option<&str>) -> String {
        let mut prompt = String::new();
        if let Some(file_path) = file_path {
            prompt.push_str(&format!("File: {file_path}\n"));
        }
        if let Some(language_name) = language_name {
            prompt.push_str(&format!("Language: {language_name}\n"));
        }
        prompt.push_str(&format!(
            "\n<|file_start|>\n{}{CURSOR_MARKER}{}\n<|file_end|>",
            self.prefix, self.suffix
        ));
        prompt
    }
}

/// Keeps at most `max_bytes` from the end of `text`, starting on a line boundary when possible.
fn keep_tail(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = text.ceil_char_boundary(text.len() - max_bytes);
    if let Some(newline) = text[cut..].find('\n') {
        cut += newline + 1;
    }
    text[cut..].to_string()
}

/// Keeps at most `max_bytes` from the start of `text`, ending on a line boundary when possible.
fn keep_head(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = text.floor_char_boundary(max_bytes);
    if let Some(newline) = text[..cut].rfind('\n') {
        cut = newline;
    }
    text[..cut].to_string()
}

/// Turns the raw model reply into the text inserted at the cursor, or `None` if there is nothing
/// worth showing.
fn clean_completion(raw: &str, excerpt: &Excerpt) -> Option<String> {
    let mut completion = strip_code_fence(raw).replace(CURSOR_MARKER, "");

    // Chat models sometimes restate what was already typed on the cursor line.
    let typed_on_line = excerpt
        .prefix
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim_start();
    if typed_on_line.len() >= 3
        && let Some(rest) = completion.trim_start().strip_prefix(typed_on_line)
    {
        completion = rest.to_string();
    }

    let completion = completion.trim_end();

    // ...or repeat the rest of the cursor line, e.g. a closing `)` that already exists.
    let rest_of_line = excerpt.suffix.split('\n').next().unwrap_or_default().trim();
    let completion = match completion.strip_suffix(rest_of_line) {
        Some(stripped)
            if !rest_of_line.is_empty() && is_balanced(stripped) && !is_balanced(completion) =>
        {
            stripped.trim_end()
        }
        _ => completion,
    };

    if completion.trim().is_empty() {
        None
    } else {
        Some(completion.to_string())
    }
}

fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim_matches(|character| character == '\n' || character == '\r');
    let Some(after_fence) = trimmed.strip_prefix("```") else {
        return raw;
    };
    let Some((_language_tag, body)) = after_fence.split_once('\n') else {
        return raw;
    };
    body.trim_end()
        .strip_suffix("```")
        .unwrap_or(body)
        .trim_end_matches(['\n', '\r'])
}

fn is_balanced(text: &str) -> bool {
    [('(', ')'), ('[', ']'), ('{', '}')]
        .iter()
        .all(|(open, close)| {
            text.chars().filter(|character| character == open).count()
                == text.chars().filter(|character| character == close).count()
        })
}

async fn fetch_completion(
    http_client: Arc<dyn HttpClient>,
    credentials: &BundledCredentials,
    options: &RequestOptions,
    user_message: String,
) -> Result<String> {
    let request = ChatRequest {
        model: &options.model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: SYSTEM_PROMPT.to_string(),
            },
            ChatMessage {
                role: "user",
                content: user_message,
            },
        ],
        max_tokens: options.max_tokens,
        temperature: 0.0,
        stream: false,
        // Reasoning roughly doubles latency and adds nothing for inline completions.
        enable_thinking: false,
    };

    let http_request = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(format!("{}/chat/completions", credentials.url))
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", credentials.key))
        .timeout(REQUEST_TIMEOUT)
        .body(http_client::AsyncBody::from(serde_json::to_string(
            &request,
        )?))?;

    let mut response = http_client.send(http_request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "DeepSeek API returned {}: {}",
            response.status(),
            body
        ));
    }

    let response: ChatResponse =
        serde_json::from_str(&body).context("unexpected DeepSeek API response")?;
    response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .context("DeepSeek API returned no completion")
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
    enable_thinking: bool,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn excerpt(prefix: &str, suffix: &str) -> Excerpt {
        Excerpt::from_parts(prefix.to_string(), suffix.to_string())
    }

    #[test]
    fn plain_completion_is_kept() {
        let excerpt = excerpt("def add(a, b):\n    ", "\n\nprint(add(1, 2))\n");
        assert_eq!(
            clean_completion("return a + b\n", &excerpt).as_deref(),
            Some("return a + b")
        );
    }

    #[test]
    fn code_fences_and_markers_are_removed() {
        let excerpt = excerpt("let x = ", ";\n");
        assert_eq!(
            clean_completion("```rust\n42<|CURSOR|>\n```\n", &excerpt).as_deref(),
            Some("42")
        );
    }

    #[test]
    fn restated_line_prefix_is_removed() {
        let excerpt = excerpt("fn main() {\n    let total = ", "\n}");
        assert_eq!(
            clean_completion("let total = items.len();", &excerpt).as_deref(),
            Some("items.len();")
        );
    }

    #[test]
    fn duplicated_closing_text_is_removed_only_when_unbalanced() {
        let excerpt = excerpt("print(", ")\n");
        assert_eq!(clean_completion("a, b)", &excerpt).as_deref(), Some("a, b"));
        assert_eq!(
            clean_completion("len(items)", &excerpt).as_deref(),
            Some("len(items)")
        );
    }

    #[test]
    fn empty_replies_yield_nothing() {
        let excerpt = excerpt("x", "");
        assert_eq!(clean_completion("  \n", &excerpt), None);
        assert_eq!(clean_completion("```\n```", &excerpt), None);
    }

    #[test]
    fn excerpt_is_cut_at_line_boundaries() {
        let prefix = format!("{}\nlast line ", "a".repeat(MAX_PREFIX_BYTES));
        let suffix = format!("end\n{}", "b".repeat(MAX_SUFFIX_BYTES));
        let excerpt = Excerpt::from_parts(prefix, suffix);
        assert_eq!(excerpt.prefix, "last line ");
        assert_eq!(excerpt.suffix, "end");
    }

    /// Decrypts the bundled key and asks the real endpoint for a completion. Needs network access
    /// and `key.enc`; run with `cargo test -p deepseek_edit_prediction -- --ignored --nocapture`.
    #[test]
    #[ignore = "calls the real DeepSeek endpoint"]
    fn live_completion() {
        let credentials = credentials().expect("bundled credentials should decrypt");
        let http_client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        let excerpt = excerpt(
            "def fibonacci(n):\n    \"\"\"Return the n-th Fibonacci number.\"\"\"\n    if n < 2:\n        return n\n    ",
            "\n\n\ndef main():\n    print(fibonacci(10))\n",
        );
        let request = RequestOptions {
            model: "deepseek-v4.1-flash".to_string(),
            max_tokens: 64,
        };
        let started_at = Instant::now();
        let raw = futures::executor::block_on(fetch_completion(
            http_client,
            &credentials,
            &request,
            excerpt.prompt(Some("demo/fib.py"), Some("Python")),
        ))
        .expect("completion request should succeed");
        let completion = clean_completion(&raw, &excerpt).expect("completion should not be empty");
        println!(
            "completion in {:.2}s: {completion:?}",
            started_at.elapsed().as_secs_f64()
        );
        assert!(completion.contains("fibonacci"), "{completion:?}");
    }

    #[test]
    fn prompt_contains_cursor_marker_and_metadata() {
        let excerpt = excerpt("let x = ", ";");
        let prompt = excerpt.prompt(Some("src/main.rs"), Some("Rust"));
        assert_eq!(
            prompt,
            "File: src/main.rs\nLanguage: Rust\n\n<|file_start|>\nlet x = <|CURSOR|>;\n<|file_end|>"
        );
    }
}
