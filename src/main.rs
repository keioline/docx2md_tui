use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use arboard::Clipboard;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use roxmltree::{Document, Node};
use zip::ZipArchive;

const DOC_XML_PATH: &str = "word/document.xml";
const DOC_RELS_PATH: &str = "word/_rels/document.xml.rels";
const IMAGE_REL_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const HISTORY_MAX: usize = 30;
const PREVIEW_SAMPLE_LINES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    InputDialog,
    PreflightSummary,
    PreviewSummary,
    HistoryList,
    Working,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputFocus {
    SourcePath,
    OutputDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputType {
    SingleFile,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    FileNotFound,
    InvalidDocx,
    PermissionDenied,
    ParseFailed,
    IoFailure,
    InvalidInput,
}

#[derive(Clone, Debug)]
struct AppError {
    kind: ErrorKind,
    message: String,
}

impl AppError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

type AppResult<T> = Result<T, AppError>;

#[derive(Clone, Copy, Debug)]
struct ConvertOptions {
    overwrite: bool,
    recursive: bool,
    keep_structure: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            recursive: false,
            keep_structure: true,
        }
    }
}

#[derive(Clone, Debug)]
struct AppConfig {
    last_input: String,
    last_output_dir: String,
    options: ConvertOptions,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_input: String::new(),
            last_output_dir: String::new(),
            options: ConvertOptions::default(),
        }
    }
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    input_path: String,
    output_dir: String,
    overwrite: bool,
    recursive: bool,
    keep_structure: bool,
}

#[derive(Clone, Debug)]
struct PlannedFile {
    src: PathBuf,
    output: PathBuf,
}

#[derive(Clone, Debug)]
struct ConversionPlan {
    input_type: InputType,
    input_path: PathBuf,
    output_dir: PathBuf,
    options: ConvertOptions,
    files: Vec<PlannedFile>,
    existing_outputs: usize,
    renamed_outputs: usize,
    risks: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct UnsupportedSummary {
    tables_converted: usize,
    tables_degraded: usize,
    images_exported: usize,
    images_unresolved: usize,
    footnotes_ignored: usize,
    equations_ignored: usize,
}

impl UnsupportedSummary {
    fn add(&mut self, rhs: &UnsupportedSummary) {
        self.tables_converted += rhs.tables_converted;
        self.tables_degraded += rhs.tables_degraded;
        self.images_exported += rhs.images_exported;
        self.images_unresolved += rhs.images_unresolved;
        self.footnotes_ignored += rhs.footnotes_ignored;
        self.equations_ignored += rhs.equations_ignored;
    }
}

#[derive(Clone, Debug)]
struct AssetFile {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct ParseStats {
    lines: usize,
    paragraphs: usize,
    headings: usize,
    list_items: usize,
}

#[derive(Clone, Debug)]
struct ParsedMarkdown {
    markdown: String,
    stats: ParseStats,
    unsupported: UnsupportedSummary,
    assets: Vec<AssetFile>,
}

#[derive(Clone, Debug)]
struct PreparedOutput {
    src: PathBuf,
    output: PathBuf,
    markdown: String,
    assets: Vec<AssetFile>,
}

#[derive(Clone, Debug)]
struct FileFailure {
    src: PathBuf,
    error: AppError,
}

#[derive(Clone, Debug)]
struct PreviewOutcome {
    total_inputs: usize,
    prepared: Vec<PreparedOutput>,
    failures: Vec<FileFailure>,
    totals: ParseStats,
    unsupported: UnsupportedSummary,
    sample_lines: Vec<String>,
    degraded_files: Vec<String>,
    ignored_files: Vec<String>,
    image_files: Vec<String>,
}

#[derive(Clone, Debug)]
struct WriteOutcome {
    wrote: usize,
    skipped: usize,
    failed: Vec<FileFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskKind {
    Preview,
    Write,
}

#[derive(Debug)]
enum WorkerMessage {
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    PreviewDone(AppResult<PreviewOutcome>),
    WriteDone(AppResult<WriteOutcome>),
}

struct RunningTask {
    kind: TaskKind,
    rx: Receiver<WorkerMessage>,
    done: usize,
    total: usize,
    current: String,
}

struct App {
    stage: Stage,
    input_path: String,
    output_dir: String,
    input_focus: InputFocus,
    options: ConvertOptions,
    status: String,
    quit: bool,
    preflight_state: ListState,
    preview_state: ListState,
    history_state: ListState,
    history_entries: Vec<HistoryEntry>,
    history_file: PathBuf,
    config_file: PathBuf,
    plan: Option<ConversionPlan>,
    preview: Option<PreviewOutcome>,
    running: Option<RunningTask>,
}

impl App {
    fn new() -> Self {
        let history_file = default_history_file();
        let config_file = default_config_file();
        let history_entries = load_history(&history_file).unwrap_or_default();
        let config = load_config(&config_file).unwrap_or_default();

        let mut preflight_state = ListState::default();
        preflight_state.select(Some(0));

        let mut preview_state = ListState::default();
        preview_state.select(Some(0));

        let mut history_state = ListState::default();
        history_state.select(if history_entries.is_empty() { None } else { Some(0) });

        let status = if history_entries.is_empty() {
            "请输入 DOCX 文件或目录路径，Enter 生成转换摘要（F2 历史）".to_string()
        } else {
            format!(
                "已加载 {} 条历史，Enter 生成转换摘要（F2 历史）",
                history_entries.len()
            )
        };

        Self {
            stage: Stage::InputDialog,
            input_path: config.last_input,
            output_dir: config.last_output_dir,
            input_focus: InputFocus::SourcePath,
            options: config.options,
            status,
            quit: false,
            preflight_state,
            preview_state,
            history_state,
            history_entries,
            history_file,
            config_file,
            plan: None,
            preview: None,
            running: None,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    run_tui(&mut app)?;
    Ok(())
}

fn run_tui(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let loop_result = event_loop(&mut terminal, app);

    disable_raw_mode().ok();
    let _ = terminal.backend_mut().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    loop_result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    while !app.quit {
        poll_worker(app);
        terminal.draw(|frame| draw_ui(frame, app))?;

        if event::poll(std::time::Duration::from_millis(150))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    handle_key_event(app, key.code, key.modifiers);
                }
                Event::Paste(text) => handle_paste(app, text),
                _ => {}
            }
        }
    }
    Ok(())
}

fn handle_key_event(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('c') => {
                app.quit = true;
                return;
            }
            KeyCode::Char('v') => {
                if let Err(err) = paste_from_system_clipboard(app) {
                    app.status = format!("读取剪贴板失败: {}", format_app_error(&err));
                }
                return;
            }
            _ => {}
        }
    }

    match app.stage {
        Stage::InputDialog => handle_input_dialog_keys(app, code),
        Stage::PreflightSummary => handle_preflight_keys(app, code),
        Stage::PreviewSummary => handle_preview_keys(app, code),
        Stage::HistoryList => handle_history_keys(app, code),
        Stage::Working => handle_working_keys(app, code),
    }
}

fn handle_input_dialog_keys(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::F(2) => open_history(app),
        KeyCode::F(3) => {
            app.options.overwrite = !app.options.overwrite;
            app.status = format!(
                "覆盖已切换为 {}",
                if app.options.overwrite { "开启" } else { "关闭" }
            );
        }
        KeyCode::F(4) => {
            app.options.recursive = !app.options.recursive;
            app.status = format!(
                "目录递归已切换为 {}",
                if app.options.recursive { "开启" } else { "关闭" }
            );
        }
        KeyCode::F(5) => {
            app.options.keep_structure = !app.options.keep_structure;
            app.status = format!(
                "批量保留目录结构已切换为 {}",
                if app.options.keep_structure {
                    "开启"
                } else {
                    "关闭（平铺命名）"
                }
            );
        }
        KeyCode::Tab => {
            app.input_focus = match app.input_focus {
                InputFocus::SourcePath => InputFocus::OutputDir,
                InputFocus::OutputDir => InputFocus::SourcePath,
            };
        }
        KeyCode::BackTab => {
            app.input_focus = match app.input_focus {
                InputFocus::SourcePath => InputFocus::OutputDir,
                InputFocus::OutputDir => InputFocus::SourcePath,
            };
        }
        KeyCode::Backspace => match app.input_focus {
            InputFocus::SourcePath => {
                app.input_path.pop();
            }
            InputFocus::OutputDir => {
                app.output_dir.pop();
            }
        },
        KeyCode::Enter => match build_conversion_plan(
            &app.input_path,
            &app.output_dir,
            app.options,
        ) {
            Ok(plan) => {
                app.plan = Some(plan);
                app.preflight_state.select(Some(0));
                app.stage = Stage::PreflightSummary;
                app.status = "请查看转换前摘要并确认".to_string();
            }
            Err(err) => {
                app.status = format_app_error(&err);
            }
        },
        KeyCode::Char(c) => match app.input_focus {
            InputFocus::SourcePath => app.input_path.push(c),
            InputFocus::OutputDir => app.output_dir.push(c),
        },
        _ => {}
    }
}

fn handle_preflight_keys(app: &mut App, code: KeyCode) {
    const ITEM_COUNT: usize = 4;
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            app.stage = Stage::InputDialog;
            app.status = "返回输入界面".to_string();
        }
        KeyCode::Up => {
            let next = app.preflight_state.selected().unwrap_or(0).saturating_sub(1);
            app.preflight_state.select(Some(next));
        }
        KeyCode::Down | KeyCode::Tab => {
            let idx = app.preflight_state.selected().unwrap_or(0);
            app.preflight_state
                .select(Some(if idx + 1 >= ITEM_COUNT { 0 } else { idx + 1 }));
        }
        KeyCode::BackTab => {
            let idx = app.preflight_state.selected().unwrap_or(0);
            app.preflight_state
                .select(Some(if idx == 0 { ITEM_COUNT - 1 } else { idx - 1 }));
        }
        KeyCode::Enter => match app.preflight_state.selected().unwrap_or(0) {
            0 => {
                if let Some(plan) = app.plan.clone() {
                    save_session(app);
                    start_preview_task(app, plan);
                } else {
                    app.status = "缺少转换计划，请返回重新输入".to_string();
                }
            }
            1 => {
                app.stage = Stage::InputDialog;
                app.status = "返回输入界面，可继续修改".to_string();
            }
            2 => open_history(app),
            3 => app.quit = true,
            _ => {}
        },
        _ => {}
    }
}

fn handle_preview_keys(app: &mut App, code: KeyCode) {
    const ITEM_COUNT: usize = 4;
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            app.stage = Stage::InputDialog;
            app.status = "返回输入界面".to_string();
        }
        KeyCode::Up => {
            let next = app.preview_state.selected().unwrap_or(0).saturating_sub(1);
            app.preview_state.select(Some(next));
        }
        KeyCode::Down | KeyCode::Tab => {
            let idx = app.preview_state.selected().unwrap_or(0);
            app.preview_state
                .select(Some(if idx + 1 >= ITEM_COUNT { 0 } else { idx + 1 }));
        }
        KeyCode::BackTab => {
            let idx = app.preview_state.selected().unwrap_or(0);
            app.preview_state
                .select(Some(if idx == 0 { ITEM_COUNT - 1 } else { idx - 1 }));
        }
        KeyCode::Enter => match app.preview_state.selected().unwrap_or(0) {
            0 => {
                if let (Some(plan), Some(preview)) = (app.plan.clone(), app.preview.clone()) {
                    start_write_task(app, plan.options.overwrite, preview.prepared);
                } else {
                    app.status = "缺少预览数据，请先重新生成摘要".to_string();
                }
            }
            1 => {
                app.stage = Stage::InputDialog;
                app.status = "返回输入界面，可继续修改".to_string();
            }
            2 => {
                if let Some(plan) = app.plan.clone() {
                    start_preview_task(app, plan);
                } else {
                    app.status = "缺少转换计划，请返回重新输入".to_string();
                }
            }
            3 => app.quit = true,
            _ => {}
        },
        _ => {}
    }
}

fn handle_history_keys(app: &mut App, code: KeyCode) {
    if app.history_entries.is_empty() {
        app.stage = Stage::InputDialog;
        app.status = "暂无历史记录".to_string();
        return;
    }

    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            app.stage = Stage::InputDialog;
            app.status = "已返回输入界面".to_string();
        }
        KeyCode::Up => {
            let idx = app.history_state.selected().unwrap_or(0);
            let next = if idx == 0 {
                app.history_entries.len() - 1
            } else {
                idx - 1
            };
            app.history_state.select(Some(next));
        }
        KeyCode::Down | KeyCode::Tab => {
            let idx = app.history_state.selected().unwrap_or(0);
            app.history_state
                .select(Some((idx + 1) % app.history_entries.len()));
        }
        KeyCode::BackTab => {
            let idx = app.history_state.selected().unwrap_or(0);
            let next = if idx == 0 {
                app.history_entries.len() - 1
            } else {
                idx - 1
            };
            app.history_state.select(Some(next));
        }
        KeyCode::Delete => {
            let idx = app.history_state.selected().unwrap_or(0);
            if idx < app.history_entries.len() {
                app.history_entries.remove(idx);
                if app.history_entries.is_empty() {
                    app.history_state.select(None);
                    app.stage = Stage::InputDialog;
                    app.status = "历史已清空，返回输入界面".to_string();
                } else {
                    app.history_state
                        .select(Some(idx.min(app.history_entries.len() - 1)));
                    app.status = "已删除该历史项".to_string();
                }
                if let Err(err) = save_history(&app.history_file, &app.history_entries) {
                    app.status = format!("保存历史失败: {}", format_app_error(&err));
                }
            }
        }
        KeyCode::Enter => {
            let idx = app.history_state.selected().unwrap_or(0);
            if let Some(item) = app.history_entries.get(idx) {
                app.input_path = item.input_path.clone();
                app.output_dir = item.output_dir.clone();
                app.options.overwrite = item.overwrite;
                app.options.recursive = item.recursive;
                app.options.keep_structure = item.keep_structure;
                app.stage = Stage::InputDialog;
                app.status = "已从历史回填输入、输出和选项".to_string();
            }
        }
        _ => {}
    }
}

fn handle_working_keys(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.status = "任务执行中，暂不支持中断。请等待完成。".to_string();
        }
        _ => {}
    }
}

fn handle_paste(app: &mut App, text: String) {
    if app.stage != Stage::InputDialog {
        app.status = "请先返回输入界面再粘贴".to_string();
        return;
    }
    let cleaned = text
        .trim_matches(|c| c == '\r' || c == '\n' || c == '"')
        .trim()
        .to_string();
    if cleaned.is_empty() {
        app.status = "粘贴内容为空".to_string();
        return;
    }

    match app.input_focus {
        InputFocus::SourcePath => {
            app.input_path = cleaned;
            app.status = "已粘贴输入路径".to_string();
        }
        InputFocus::OutputDir => {
            app.output_dir = cleaned;
            app.status = "已粘贴输出目录".to_string();
        }
    }
}

fn paste_from_system_clipboard(app: &mut App) -> AppResult<()> {
    if app.stage != Stage::InputDialog {
        return Err(AppError::new(ErrorKind::InvalidInput, "请先返回输入界面再粘贴"));
    }
    let mut clipboard =
        Clipboard::new().map_err(|e| AppError::new(ErrorKind::IoFailure, format!("{e}")))?;
    let text = clipboard
        .get_text()
        .map_err(|e| AppError::new(ErrorKind::IoFailure, format!("{e}")))?;
    handle_paste(app, text);
    Ok(())
}

fn poll_worker(app: &mut App) {
    let mut completed: Option<WorkerMessage> = None;
    if let Some(running) = app.running.as_mut() {
        loop {
            match running.rx.try_recv() {
                Ok(msg) => match msg {
                    WorkerMessage::Progress {
                        done,
                        total,
                        current,
                    } => {
                        running.done = done;
                        running.total = total;
                        running.current = current;
                    }
                    WorkerMessage::PreviewDone(_) | WorkerMessage::WriteDone(_) => {
                        completed = Some(msg);
                        break;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.stage = Stage::InputDialog;
                    app.running = None;
                    app.status = "后台任务异常结束".to_string();
                    return;
                }
            }
        }
    }

    if let Some(message) = completed {
        app.running = None;
        match message {
            WorkerMessage::PreviewDone(result) => match result {
                Ok(outcome) => {
                    let ok = outcome.prepared.len();
                    let fail = outcome.failures.len();
                    app.preview = Some(outcome);
                    app.preview_state.select(Some(0));
                    app.stage = Stage::PreviewSummary;
                    app.status = format!("预览完成：可转换 {}，失败 {}。请确认是否写入。", ok, fail);
                }
                Err(err) => {
                    app.stage = Stage::PreflightSummary;
                    app.status = format!("预览失败：{}", format_app_error(&err));
                }
            },
            WorkerMessage::WriteDone(result) => match result {
                Ok(outcome) => {
                    let failed = outcome.failed.len();
                    app.stage = Stage::PreviewSummary;
                    app.status = format!(
                        "写入完成：成功 {}，跳过 {}，失败 {}",
                        outcome.wrote, outcome.skipped, failed
                    );
                    if let Some(plan) = app.plan.clone() {
                        let _ = remember_history(app, &plan);
                    }
                }
                Err(err) => {
                    app.stage = Stage::PreviewSummary;
                    app.status = format!("写入失败：{}", format_app_error(&err));
                }
            },
            _ => {}
        }
    }
}

fn start_preview_task(app: &mut App, plan: ConversionPlan) {
    let (tx, rx) = mpsc::channel();
    app.stage = Stage::Working;
    app.preview = None;
    app.running = Some(RunningTask {
        kind: TaskKind::Preview,
        rx,
        done: 0,
        total: plan.files.len().max(1),
        current: "准备分析 DOCX".to_string(),
    });
    app.status = "正在生成转换预览...".to_string();

    thread::spawn(move || {
        let result = generate_preview(plan, &tx);
        let _ = tx.send(WorkerMessage::PreviewDone(result));
    });
}

fn start_write_task(app: &mut App, overwrite: bool, prepared: Vec<PreparedOutput>) {
    let (tx, rx) = mpsc::channel();
    app.stage = Stage::Working;
    app.running = Some(RunningTask {
        kind: TaskKind::Write,
        rx,
        done: 0,
        total: prepared.len().max(1),
        current: "准备写入 Markdown".to_string(),
    });
    app.status = "正在写入输出文件...".to_string();

    thread::spawn(move || {
        let result = write_outputs(prepared, overwrite, &tx);
        let _ = tx.send(WorkerMessage::WriteDone(result));
    });
}

fn generate_preview(plan: ConversionPlan, tx: &Sender<WorkerMessage>) -> AppResult<PreviewOutcome> {
    if plan.files.is_empty() {
        return Err(AppError::new(
            ErrorKind::InvalidInput,
            "没有可处理的 DOCX 文件",
        ));
    }

    let total = plan.files.len();
    let mut prepared = Vec::new();
    let mut failures = Vec::new();
    let mut totals = ParseStats::default();
    let mut unsupported = UnsupportedSummary::default();
    let mut sample_lines = Vec::new();
    let mut degraded_files = Vec::new();
    let mut ignored_files = Vec::new();
    let mut image_files = Vec::new();

    for (idx, file) in plan.files.iter().enumerate() {
        let current = format!(
            "分析中 ({}/{}) {}",
            idx + 1,
            total,
            file.src.display()
        );
        let _ = tx.send(WorkerMessage::Progress {
            done: idx,
            total,
            current,
        });

        match extract_markdown_from_docx(&file.src) {
            Ok(parsed) => {
                if sample_lines.is_empty() {
                    sample_lines = parsed
                        .markdown
                        .lines()
                        .take(PREVIEW_SAMPLE_LINES)
                        .map(|s| s.to_string())
                        .collect();
                }
                totals.lines += parsed.stats.lines;
                totals.paragraphs += parsed.stats.paragraphs;
                totals.headings += parsed.stats.headings;
                totals.list_items += parsed.stats.list_items;
                unsupported.add(&parsed.unsupported);
                if parsed.unsupported.tables_degraded > 0 {
                    degraded_files.push(format!(
                        "{} (复杂表格降级 {} 个)",
                        file.src.display(),
                        parsed.unsupported.tables_degraded
                    ));
                }
                if parsed.unsupported.images_unresolved > 0
                    || parsed.unsupported.footnotes_ignored > 0
                    || parsed.unsupported.equations_ignored > 0
                {
                    ignored_files.push(format!(
                        "{} (图片未解析 {} / 脚注忽略 {} / 公式忽略 {})",
                        file.src.display(),
                        parsed.unsupported.images_unresolved,
                        parsed.unsupported.footnotes_ignored,
                        parsed.unsupported.equations_ignored
                    ));
                }
                if parsed.unsupported.images_exported > 0 {
                    image_files.push(format!(
                        "{} (提取图片 {} 个)",
                        file.src.display(),
                        parsed.unsupported.images_exported
                    ));
                }
                prepared.push(PreparedOutput {
                    src: file.src.clone(),
                    output: file.output.clone(),
                    markdown: parsed.markdown,
                    assets: parsed.assets,
                });
            }
            Err(err) => failures.push(FileFailure {
                src: file.src.clone(),
                error: err,
            }),
        }

        let _ = tx.send(WorkerMessage::Progress {
            done: idx + 1,
            total,
            current: format!("已完成 {}/{}", idx + 1, total),
        });
    }

    Ok(PreviewOutcome {
        total_inputs: total,
        prepared,
        failures,
        totals,
        unsupported,
        sample_lines,
        degraded_files,
        ignored_files,
        image_files,
    })
}

fn write_outputs(
    prepared: Vec<PreparedOutput>,
    overwrite: bool,
    tx: &Sender<WorkerMessage>,
) -> AppResult<WriteOutcome> {
    let total = prepared.len();
    if total == 0 {
        return Err(AppError::new(
            ErrorKind::InvalidInput,
            "没有可写入的文件，请先检查预览失败项",
        ));
    }

    let mut wrote = 0usize;
    let mut skipped = 0usize;
    let mut failed = Vec::new();

    for (idx, item) in prepared.into_iter().enumerate() {
        let _ = tx.send(WorkerMessage::Progress {
            done: idx,
            total,
            current: format!(
                "写入中 ({}/{}) {}",
                idx + 1,
                total,
                item.output.display()
            ),
        });

        let output = item.output;
        if output.exists() && !overwrite {
            skipped += 1;
            let _ = tx.send(WorkerMessage::Progress {
                done: idx + 1,
                total,
                current: format!("跳过已存在文件：{}", output.display()),
            });
            continue;
        }

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(err) = fs::create_dir_all(parent) {
                    failed.push(FileFailure {
                        src: item.src,
                        error: map_io_error(err, &format!("无法创建目录: {}", parent.display())),
                    });
                    continue;
                }
            }
        }

        let mut item_failed = false;
        for asset in &item.assets {
            let asset_path = output
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&asset.relative_path);
            if let Some(asset_parent) = asset_path.parent() {
                if !asset_parent.as_os_str().is_empty() {
                    if let Err(err) = fs::create_dir_all(asset_parent) {
                        failed.push(FileFailure {
                            src: item.src.clone(),
                            error: map_io_error(
                                err,
                                &format!("无法创建图片目录: {}", asset_parent.display()),
                            ),
                        });
                        item_failed = true;
                        break;
                    }
                }
            }
            if let Err(err) = fs::write(&asset_path, &asset.bytes) {
                failed.push(FileFailure {
                    src: item.src.clone(),
                    error: map_io_error(err, &format!("图片写入失败: {}", asset_path.display())),
                });
                item_failed = true;
                break;
            }
        }
        if item_failed {
            continue;
        }

        match fs::write(&output, item.markdown.as_bytes()) {
            Ok(_) => wrote += 1,
            Err(err) => failed.push(FileFailure {
                src: item.src,
                error: map_io_error(err, &format!("写入失败: {}", output.display())),
            }),
        }

        let _ = tx.send(WorkerMessage::Progress {
            done: idx + 1,
            total,
            current: format!("已完成 {}/{}", idx + 1, total),
        });
    }

    Ok(WriteOutcome {
        wrote,
        skipped,
        failed,
    })
}

fn open_history(app: &mut App) {
    if app.history_entries.is_empty() {
        app.status = "暂无历史记录".to_string();
    } else {
        app.stage = Stage::HistoryList;
        app.history_state.select(Some(0));
        app.status = "历史列表：Enter 回填，Delete 删除，Esc 返回".to_string();
    }
}

fn save_session(app: &mut App) {
    let config = AppConfig {
        last_input: sanitize_field(&app.input_path),
        last_output_dir: sanitize_field(&app.output_dir),
        options: app.options,
    };
    if let Err(err) = save_config(&app.config_file, &config) {
        app.status = format!("保存配置失败: {}", format_app_error(&err));
    }
}

fn remember_history(app: &mut App, plan: &ConversionPlan) -> AppResult<()> {
    let input = sanitize_field(plan.input_path.to_string_lossy().as_ref());
    let output = sanitize_field(plan.output_dir.to_string_lossy().as_ref());
    if input.is_empty() || output.is_empty() {
        return Ok(());
    }
    app.history_entries.retain(|h| {
        !(h.input_path.eq_ignore_ascii_case(&input)
            && h.output_dir.eq_ignore_ascii_case(&output)
            && h.overwrite == plan.options.overwrite
            && h.recursive == plan.options.recursive
            && h.keep_structure == plan.options.keep_structure)
    });
    app.history_entries.insert(
        0,
        HistoryEntry {
            input_path: input,
            output_dir: output,
            overwrite: plan.options.overwrite,
            recursive: plan.options.recursive,
            keep_structure: plan.options.keep_structure,
        },
    );
    if app.history_entries.len() > HISTORY_MAX {
        app.history_entries.truncate(HISTORY_MAX);
    }
    app.history_state.select(Some(0));
    save_history(&app.history_file, &app.history_entries)
}

fn draw_ui(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1), Constraint::Length(4)])
        .margin(1)
        .split(frame.area());

    let title = Paragraph::new("DOCX -> Markdown Converter (Rust TUI)")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("标题"));
    frame.render_widget(title, root[0]);

    match app.stage {
        Stage::InputDialog => draw_input_dialog(frame, root[1], app),
        Stage::PreflightSummary => draw_preflight(frame, root[1], app),
        Stage::PreviewSummary => draw_preview(frame, root[1], app),
        Stage::HistoryList => draw_history(frame, root[1], app),
        Stage::Working => draw_working(frame, root[1], app),
    }

    let footer = Paragraph::new(vec![
        Line::from("快捷键: Ctrl+V 粘贴 | F2 历史 | F3 覆盖 | F4 递归 | F5 保留结构 | q/Esc 退出"),
        Line::from(app.status.as_str()),
    ])
    .block(Block::default().borders(Borders::ALL).title("状态"));
    frame.render_widget(footer, root[2]);
}

fn draw_input_dialog(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(92, 82, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("输入与选项"),
        popup,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Min(1),
        ])
        .margin(1)
        .split(popup);

    let input_style = if app.input_focus == InputFocus::SourcePath {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let output_style = if app.input_focus == InputFocus::OutputDir {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    frame.render_widget(
        Paragraph::new(app.input_path.as_str())
            .style(input_style)
            .block(Block::default().borders(Borders::ALL).title("输入路径（文件或目录）")),
        chunks[0],
    );

    let output_hint = if app.output_dir.trim().is_empty() {
        "（为空时自动推导：文件同目录 / 目录输出到 <输入目录>_md）".to_string()
    } else {
        app.output_dir.clone()
    };
    frame.render_widget(
        Paragraph::new(output_hint)
            .style(output_style)
            .block(Block::default().borders(Borders::ALL).title("输出目录")),
        chunks[1],
    );

    let option_lines = vec![
        Line::from(format!(
            "覆盖已存在文件(F3): {}",
            if app.options.overwrite { "开启" } else { "关闭" }
        )),
        Line::from(format!(
            "目录递归扫描(F4): {}",
            if app.options.recursive { "开启" } else { "关闭" }
        )),
        Line::from(format!(
            "批量保留目录结构(F5): {}",
            if app.options.keep_structure {
                "开启"
            } else {
                "关闭（平铺命名）"
            }
        )),
    ];
    frame.render_widget(
        Paragraph::new(option_lines).block(Block::default().borders(Borders::ALL).title("选项")),
        chunks[2],
    );

    let naming = Paragraph::new(vec![
        Line::from("命名策略："),
        Line::from("单文件 -> 输出目录/<文件名>.md"),
        Line::from("目录批量 + 保留结构 -> 输出目录/<相对路径>.md"),
        Line::from("目录批量 + 平铺命名 -> 输出目录/<文件名>[_序号].md"),
    ])
    .block(Block::default().borders(Borders::ALL).title("输出规则"));
    frame.render_widget(naming, chunks[3]);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Tab 切换输入框，Enter 生成转换前摘要"),
            Line::from("支持输入 .docx 文件路径或目录路径"),
        ])
        .block(Block::default().borders(Borders::ALL).title("说明")),
        chunks[4],
    );
}

fn draw_preflight(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(92, 84, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("转换前摘要"),
        popup,
    );

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Length(8), Constraint::Min(3)])
        .margin(1)
        .split(popup);

    let Some(plan) = app.plan.as_ref() else {
        frame.render_widget(
            Paragraph::new("暂无转换计划，请返回输入界面")
                .block(Block::default().borders(Borders::ALL).title("错误")),
            inner[0],
        );
        return;
    };

    let mut summary_lines = vec![
        Line::from(format!("输入路径: {}", plan.input_path.display())),
        Line::from(format!("输出目录: {}", plan.output_dir.display())),
        Line::from(format!(
            "输入类型: {}",
            if plan.input_type == InputType::SingleFile {
                "单文件"
            } else {
                "目录批量"
            }
        )),
        Line::from(format!("待处理文件: {}", plan.files.len())),
        Line::from(format!(
            "关键选项: 覆盖={} / 递归={} / 保留结构={}",
            bool_zh(plan.options.overwrite),
            bool_zh(plan.options.recursive),
            bool_zh(plan.options.keep_structure)
        )),
        Line::from(format!(
            "输出冲突提示: 已存在 {} 个，平铺重命名 {} 个",
            plan.existing_outputs, plan.renamed_outputs
        )),
    ];
    if let Some(first) = plan.files.first() {
        summary_lines.push(Line::from(format!("示例输出: {}", first.output.display())));
    }

    frame.render_widget(
        Paragraph::new(summary_lines).block(Block::default().borders(Borders::ALL).title("摘要")),
        inner[0],
    );

    let mut risk_lines = vec![
        Line::from("边界说明："),
        Line::from("保留: 段落、Heading1..6、简单列表"),
        Line::from("降级: 制表符/换行为纯文本"),
        Line::from("忽略: 图片、表格、脚注/尾注、公式"),
    ];
    if plan.risks.is_empty() {
        risk_lines.push(Line::from("风险提示: 暂无明显风险"));
    } else {
        for r in &plan.risks {
            risk_lines.push(Line::from(format!("风险提示: {r}")));
        }
    }
    frame.render_widget(
        Paragraph::new(risk_lines).block(Block::default().borders(Borders::ALL).title("风险与边界")),
        inner[1],
    );

    let items = vec![
        ListItem::new("生成转换结果预览（解析但暂不写盘）"),
        ListItem::new("返回修改输入或选项"),
        ListItem::new("从历史回填路径和选项"),
        ListItem::new("退出"),
    ];
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    let mut state = app.preflight_state.clone();
    frame.render_stateful_widget(list, inner[2], &mut state);
}

fn draw_preview(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(92, 86, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("转换结果预览"),
        popup,
    );

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Length(8), Constraint::Min(4)])
        .margin(1)
        .split(popup);

    let Some(preview) = app.preview.as_ref() else {
        frame.render_widget(
            Paragraph::new("暂无预览结果，请先生成摘要")
                .block(Block::default().borders(Borders::ALL).title("提示")),
            inner[0],
        );
        return;
    };

    let success = preview.prepared.len();
    let failed = preview.failures.len();
    let mut summary = vec![
        Line::from(format!(
            "输入文件总数: {} | 可写入: {} | 失败: {}",
            preview.total_inputs, success, failed
        )),
        Line::from(format!(
            "统计: 行数 {} / 段落 {} / 标题 {} / 列表 {}",
            preview.totals.lines,
            preview.totals.paragraphs,
            preview.totals.headings,
            preview.totals.list_items
        )),
        Line::from(format!(
            "边界统计: 表格已转 {} / 表格降级 {} / 图片导出 {} / 图片未解析 {}",
            preview.unsupported.tables_converted,
            preview.unsupported.tables_degraded,
            preview.unsupported.images_exported,
            preview.unsupported.images_unresolved
        )),
        Line::from(format!(
            "忽略统计: 脚注 {} / 公式 {}",
            preview.unsupported.footnotes_ignored,
            preview.unsupported.equations_ignored
        )),
    ];
    if !preview.sample_lines.is_empty() {
        summary.push(Line::from("示例输出（首个成功文件前几行）："));
        for line in &preview.sample_lines {
            summary.push(Line::from(format!("  {line}")));
        }
    }
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title("预览摘要")),
        inner[0],
    );

    let mut failure_lines = vec![Line::from("失败和边界明细（最多各显示 2 条）：")];
    if preview.failures.is_empty()
        && preview.degraded_files.is_empty()
        && preview.ignored_files.is_empty()
        && preview.image_files.is_empty()
    {
        failure_lines.push(Line::from("无失败项，且无降级/忽略项"));
    } else {
        for item in preview.failures.iter().take(2) {
            failure_lines.push(Line::from(format!(
                "失败: {} -> {}",
                item.src.display(),
                format_app_error(&item.error)
            )));
        }
        if preview.failures.len() > 2 {
            failure_lines.push(Line::from(format!(
                "... 其余失败 {} 条请调整后重试",
                preview.failures.len() - 2
            )));
        }
        for line in preview.degraded_files.iter().take(2) {
            failure_lines.push(Line::from(format!("降级: {line}")));
        }
        if preview.degraded_files.len() > 2 {
            failure_lines.push(Line::from(format!(
                "... 其余降级 {} 条",
                preview.degraded_files.len() - 2
            )));
        }
        for line in preview.ignored_files.iter().take(2) {
            failure_lines.push(Line::from(format!("忽略: {line}")));
        }
        if preview.ignored_files.len() > 2 {
            failure_lines.push(Line::from(format!(
                "... 其余忽略 {} 条",
                preview.ignored_files.len() - 2
            )));
        }
        for line in preview.image_files.iter().take(2) {
            failure_lines.push(Line::from(format!("图片: {line}")));
        }
        if preview.image_files.len() > 2 {
            failure_lines.push(Line::from(format!(
                "... 其余图片提取 {} 条",
                preview.image_files.len() - 2
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(failure_lines)
            .block(Block::default().borders(Borders::ALL).title("失败与风险提示")),
        inner[1],
    );

    let items = vec![
        ListItem::new("确认写入输出文件"),
        ListItem::new("返回输入界面调整后重试"),
        ListItem::new("重新生成预览摘要"),
        ListItem::new("退出"),
    ];
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    let mut state = app.preview_state.clone();
    frame.render_stateful_widget(list, inner[2], &mut state);
}

fn draw_history(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(94, 84, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("历史路径与选项"),
        popup,
    );

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .margin(1)
        .split(popup);

    frame.render_widget(
        Paragraph::new("Enter 回填 | Delete 删除 | Esc 返回")
            .block(Block::default().borders(Borders::ALL).title("操作")),
        inner[0],
    );

    let items: Vec<ListItem> = app
        .history_entries
        .iter()
        .enumerate()
        .map(|(i, item)| {
            ListItem::new(format!(
                "{}. {} | out={} | 覆盖:{} 递归:{} 保留结构:{}",
                i + 1,
                item.input_path,
                item.output_dir,
                bool_zh(item.overwrite),
                bool_zh(item.recursive),
                bool_zh(item.keep_structure)
            ))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    let mut state = app.history_state.clone();
    frame.render_stateful_widget(list, inner[1], &mut state);
}

fn draw_working(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(80, 50, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("任务执行中"),
        popup,
    );

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3), Constraint::Min(1)])
        .margin(1)
        .split(popup);

    if let Some(running) = app.running.as_ref() {
        let title = if running.kind == TaskKind::Preview {
            "正在生成预览摘要"
        } else {
            "正在写入输出文件"
        };
        frame.render_widget(
            Paragraph::new(title).block(Block::default().borders(Borders::ALL).title("阶段")),
            inner[0],
        );

        let ratio = if running.total == 0 {
            0.0
        } else {
            running.done as f64 / running.total as f64
        };
        frame.render_widget(
            Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("进度"))
                .ratio(ratio.min(1.0))
                .label(format!("{}/{}", running.done, running.total)),
            inner[1],
        );

        frame.render_widget(
            Paragraph::new(running.current.as_str())
                .block(Block::default().borders(Borders::ALL).title("当前任务")),
            inner[2],
        );
    } else {
        frame.render_widget(
            Paragraph::new("任务启动中...").block(Block::default().borders(Borders::ALL)),
            inner[0],
        );
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);

    horizontal[1]
}

fn build_conversion_plan(
    input_raw: &str,
    output_raw: &str,
    options: ConvertOptions,
) -> AppResult<ConversionPlan> {
    let input = sanitize_path_input(input_raw);
    if input.is_empty() {
        return Err(AppError::new(ErrorKind::InvalidInput, "输入路径不能为空"));
    }
    let input_path = PathBuf::from(&input);
    if !input_path.exists() {
        return Err(AppError::new(
            ErrorKind::FileNotFound,
            format!("文件或目录不存在: {}", input_path.display()),
        ));
    }

    let metadata = fs::metadata(&input_path).map_err(|e| {
        map_io_error(
            e,
            format!("无法读取输入路径信息: {}", input_path.display()).as_str(),
        )
    })?;

    let input_type = if metadata.is_file() {
        if !is_docx_path(&input_path) {
            return Err(AppError::new(
                ErrorKind::InvalidInput,
                "输入文件不是 .docx，请提供 docx 文件路径或目录路径",
            ));
        }
        InputType::SingleFile
    } else if metadata.is_dir() {
        InputType::Directory
    } else {
        return Err(AppError::new(
            ErrorKind::InvalidInput,
            "输入路径既不是文件也不是目录",
        ));
    };

    let output_dir = resolve_output_dir(&input_path, input_type, output_raw);
    let files = match input_type {
        InputType::SingleFile => {
            let output = output_dir.join(
                input_path
                    .file_stem()
                    .map(|s| s.to_os_string())
                    .unwrap_or_else(|| "output".into()),
            );
            let mut out = output;
            out.set_extension("md");
            vec![PlannedFile {
                src: input_path.clone(),
                output: out,
            }]
        }
        InputType::Directory => build_batch_files(&input_path, &output_dir, options)?,
    };

    if files.is_empty() {
        return Err(AppError::new(
            ErrorKind::InvalidInput,
            "目录中未找到可转换的 .docx 文件",
        ));
    }

    let existing_outputs = files.iter().filter(|f| f.output.exists()).count();
    let renamed_outputs = if input_type == InputType::Directory && !options.keep_structure {
        count_renamed_outputs(&files)
    } else {
        0
    };

    let mut risks = Vec::new();
    if files.len() > 100 {
        risks.push(format!("文件数量较多（{}），转换可能耗时", files.len()));
    }
    if existing_outputs > 0 && !options.overwrite {
        risks.push(format!(
            "检测到 {} 个已有输出文件，将在写入阶段自动跳过",
            existing_outputs
        ));
    }
    if input_type == InputType::Directory && !options.recursive {
        risks.push("目录模式未开启递归，仅处理顶层 .docx 文件".to_string());
    }
    if renamed_outputs > 0 {
        risks.push(format!(
            "平铺命名存在重名，已为 {} 个文件追加序号后缀",
            renamed_outputs
        ));
    }

    Ok(ConversionPlan {
        input_type,
        input_path,
        output_dir,
        options,
        files,
        existing_outputs,
        renamed_outputs,
        risks,
    })
}

fn resolve_output_dir(input_path: &Path, input_type: InputType, output_raw: &str) -> PathBuf {
    let output = sanitize_path_input(output_raw);
    if !output.is_empty() {
        return PathBuf::from(output);
    }
    match input_type {
        InputType::SingleFile => input_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
        InputType::Directory => {
            let base = input_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "output".to_string());
            input_path.with_file_name(format!("{base}_md"))
        }
    }
}

fn build_batch_files(
    root: &Path,
    output_dir: &Path,
    options: ConvertOptions,
) -> AppResult<Vec<PlannedFile>> {
    let mut docs = Vec::new();
    collect_docx_files(root, options.recursive, &mut docs)?;
    docs.sort_by(|a, b| a.cmp(b));

    if docs.is_empty() {
        return Ok(Vec::new());
    }

    if options.keep_structure {
        let mut result = Vec::with_capacity(docs.len());
        for src in docs {
            let rel = src.strip_prefix(root).map_err(|_| {
                AppError::new(
                    ErrorKind::InvalidInput,
                    format!("无法计算相对路径: {}", src.display()),
                )
            })?;
            let mut out = output_dir.join(rel);
            out.set_extension("md");
            result.push(PlannedFile { src, output: out });
        }
        return Ok(result);
    }

    let mut result = Vec::with_capacity(docs.len());
    let mut name_count: HashMap<String, usize> = HashMap::new();
    for src in docs {
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "output".to_string());
        let lower = stem.to_lowercase();
        let counter = name_count.entry(lower).or_insert(0);
        *counter += 1;
        let file_name = if *counter == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}_{}.md", *counter)
        };
        let out = output_dir.join(file_name);
        result.push(PlannedFile { src, output: out });
    }
    Ok(result)
}

fn count_renamed_outputs(files: &[PlannedFile]) -> usize {
    files
        .iter()
        .filter(|f| {
            f.output
                .file_stem()
                .map(|stem| {
                    let s = stem.to_string_lossy();
                    let bytes = s.as_bytes();
                    if bytes.len() < 3 {
                        return false;
                    }
                    let mut i = bytes.len();
                    while i > 0 && bytes[i - 1].is_ascii_digit() {
                        i -= 1;
                    }
                    i > 0 && bytes[i - 1] == b'_'
                })
                .unwrap_or(false)
        })
        .count()
}

fn collect_docx_files(root: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> AppResult<()> {
    let entries = fs::read_dir(root)
        .map_err(|e| map_io_error(e, &format!("无法读取目录: {}", root.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| map_io_error(e, &format!("无法读取目录项: {}", root.display())))?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .map_err(|e| map_io_error(e, &format!("无法读取文件类型: {}", path.display())))?;
        if ty.is_file() && is_docx_path(&path) {
            out.push(path);
        } else if recursive && ty.is_dir() {
            collect_docx_files(&path, recursive, out)?;
        }
    }
    Ok(())
}

fn extract_markdown_from_docx(path: &Path) -> AppResult<ParsedMarkdown> {
    if !path.exists() {
        return Err(AppError::new(
            ErrorKind::FileNotFound,
            format!("文件不存在: {}", path.display()),
        ));
    }

    let file = File::open(path)
        .map_err(|e| map_io_error(e, &format!("无法打开文件: {}", path.display())))?;
    let mut archive = ZipArchive::new(file).map_err(|e| {
        AppError::new(
            ErrorKind::InvalidDocx,
            format!("无效 DOCX（无法读取 zip 结构）: {} ({e})", path.display()),
        )
    })?;

    let mut xml = String::new();
    {
        let mut doc_xml = archive.by_name(DOC_XML_PATH).map_err(|_| {
            AppError::new(
                ErrorKind::InvalidDocx,
                format!("无效 DOCX（缺少 {DOC_XML_PATH}）: {}", path.display()),
            )
        })?;
        doc_xml.read_to_string(&mut xml).map_err(|e| {
            map_io_error(
                e,
                &format!("读取 document.xml 失败: {}", path.display()),
            )
        })?;
    }

    let rel_map = load_image_relationships(&mut archive);
    parse_document_xml_to_markdown_with_archive(&xml, &mut archive, &rel_map).map_err(|e| {
        AppError::new(
            ErrorKind::ParseFailed,
            format!("解析失败: {} ({})", path.display(), e.message),
        )
    })
}

#[cfg(test)]
fn parse_document_xml_to_markdown(xml: &str) -> AppResult<ParsedMarkdown> {
    let rel_map = HashMap::new();
    parse_document_xml_to_markdown_core(xml, &rel_map, &mut |_entry| None)
}

fn parse_document_xml_to_markdown_with_archive<R: Read + std::io::Seek>(
    xml: &str,
    archive: &mut ZipArchive<R>,
    rel_map: &HashMap<String, String>,
) -> AppResult<ParsedMarkdown> {
    parse_document_xml_to_markdown_core(xml, rel_map, &mut |entry| {
        read_zip_entry_bytes(archive, entry)
    })
}

fn parse_document_xml_to_markdown_core(
    xml: &str,
    rel_map: &HashMap<String, String>,
    read_entry: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
) -> AppResult<ParsedMarkdown> {
    let doc = Document::parse(xml).map_err(|e| {
        AppError::new(
            ErrorKind::ParseFailed,
            format!("XML 解析失败: {e}"),
        )
    })?;

    let mut out_lines: Vec<String> = Vec::new();
    let mut stats = ParseStats::default();
    let mut unsupported = UnsupportedSummary::default();
    let mut assets = Vec::new();
    let mut image_index = 1usize;

    let body = doc
        .descendants()
        .find(|n| n.tag_name().name() == "body")
        .ok_or_else(|| AppError::new(ErrorKind::ParseFailed, "document.xml 缺少 body 节点"))?;

    for node in body.children().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            "p" => {
                parse_paragraph_block(
                    node,
                    rel_map,
                    read_entry,
                    &mut image_index,
                    &mut out_lines,
                    &mut stats,
                    &mut unsupported,
                    &mut assets,
                );
            }
            "tbl" => parse_table_block(
                node,
                &mut out_lines,
                &mut unsupported,
                &mut stats,
            ),
            _ => {}
        }
    }

    collect_ignored_features(&doc, &mut unsupported);

    while out_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        out_lines.pop();
    }

    let markdown = out_lines.join("\n");
    stats.lines = markdown.lines().count();

    Ok(ParsedMarkdown {
        markdown,
        stats,
        unsupported,
        assets,
    })
}

fn load_image_relationships<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(mut rels) = archive.by_name(DOC_RELS_PATH) else {
        return map;
    };
    let mut rel_xml = String::new();
    if rels.read_to_string(&mut rel_xml).is_err() {
        return map;
    }
    let Ok(doc) = Document::parse(&rel_xml) else {
        return map;
    };
    for rel in doc.descendants().filter(|n| n.tag_name().name() == "Relationship") {
        let Some(id) = rel
            .attribute("Id")
            .or_else(|| rel.attribute("r:Id")) else {
            continue;
        };
        let Some(typ) = rel.attribute("Type") else {
            continue;
        };
        if typ == IMAGE_REL_TYPE {
            if let Some(target) = rel.attribute("Target") {
                map.insert(id.to_string(), normalize_word_target(target));
            }
        }
    }
    map
}

fn normalize_word_target(target: &str) -> String {
    let t = target.replace('\\', "/");
    if t.starts_with("word/") {
        t
    } else if t.starts_with('/') {
        t.trim_start_matches('/').to_string()
    } else {
        format!("word/{t}")
    }
}

fn parse_paragraph_block(
    para: Node<'_, '_>,
    rel_map: &HashMap<String, String>,
    read_entry: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    image_index: &mut usize,
    out_lines: &mut Vec<String>,
    stats: &mut ParseStats,
    unsupported: &mut UnsupportedSummary,
    assets: &mut Vec<AssetFile>,
) {
    let text = paragraph_text(para);
    let images = extract_images_from_paragraph(
        para,
        rel_map,
        read_entry,
        image_index,
        unsupported,
        assets,
    );

    if text.trim().is_empty() && images.is_empty() {
        out_lines.push(String::new());
        return;
    }

    if !text.trim().is_empty() {
        stats.paragraphs += 1;
        if let Some(level) = heading_level(para) {
            out_lines.push(format!("{} {}", "#".repeat(level), text.trim()));
            out_lines.push(String::new());
            stats.headings += 1;
        } else if is_list_paragraph(para) {
            out_lines.push(format!("- {}", text.trim()));
            stats.list_items += 1;
        } else {
            out_lines.push(text.trim().to_string());
            out_lines.push(String::new());
        }
    }

    if !images.is_empty() {
        for image_line in images {
            out_lines.push(image_line);
        }
        out_lines.push(String::new());
    }
}

fn extract_images_from_paragraph(
    para: Node<'_, '_>,
    rel_map: &HashMap<String, String>,
    read_entry: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    image_index: &mut usize,
    unsupported: &mut UnsupportedSummary,
    assets: &mut Vec<AssetFile>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for blip in para.descendants().filter(|n| n.tag_name().name() == "blip") {
        let rel_id = blip
            .attributes()
            .find(|attr| attr.name() == "embed" || attr.name() == "r:embed")
            .map(|attr| attr.value().to_string());
        let Some(rel_id) = rel_id else {
            unsupported.images_unresolved += 1;
            continue;
        };
        let Some(target) = rel_map.get(&rel_id) else {
            unsupported.images_unresolved += 1;
            continue;
        };
        let Some(bytes) = read_entry(target) else {
            unsupported.images_unresolved += 1;
            continue;
        };

        let ext = Path::new(target)
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "bin".to_string());
        let file_name = format!("image_{:03}.{}", *image_index, ext);
        *image_index += 1;
        let rel = PathBuf::from("assets").join(&file_name);
        assets.push(AssetFile {
            relative_path: rel.clone(),
            bytes,
        });
        unsupported.images_exported += 1;
        lines.push(format!("![{}]({})", file_name, rel.to_string_lossy().replace('\\', "/")));
    }
    lines
}

fn read_zip_entry_bytes<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    entry_name: &str,
) -> Option<Vec<u8>> {
    let Ok(mut file) = archive.by_name(entry_name) else {
        return None;
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return None;
    }
    Some(bytes)
}

fn parse_table_block(
    table: Node<'_, '_>,
    out_lines: &mut Vec<String>,
    unsupported: &mut UnsupportedSummary,
    stats: &mut ParseStats,
) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut degraded = false;
    let mut max_cols = 0usize;

    for tr in table.children().filter(|n| n.is_element() && n.tag_name().name() == "tr") {
        let mut row = Vec::new();
        for tc in tr.children().filter(|n| n.is_element() && n.tag_name().name() == "tc") {
            if cell_has_complex_merge(tc) {
                degraded = true;
            }
            let mut cell_texts = Vec::new();
            for p in tc.descendants().filter(|n| n.tag_name().name() == "p") {
                let t = paragraph_text(p).trim().to_string();
                if !t.is_empty() {
                    cell_texts.push(t);
                }
            }
            row.push(cell_texts.join("<br>"));
        }
        if row.len() > max_cols {
            max_cols = row.len();
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }

    if rows.is_empty() {
        return;
    }

    if degraded {
        unsupported.tables_degraded += 1;
        out_lines.push("> [!NOTE] 该表格含合并单元格，已降级为纯文本块".to_string());
        for (idx, row) in rows.iter().enumerate() {
            out_lines.push(format!("行{}: {}", idx + 1, row.join(" | ")));
        }
        out_lines.push(String::new());
        return;
    }

    unsupported.tables_converted += 1;
    let mut normalized = Vec::new();
    for mut row in rows {
        if row.len() < max_cols {
            row.extend(std::iter::repeat(String::new()).take(max_cols - row.len()));
        }
        normalized.push(row);
    }
    if normalized.is_empty() {
        return;
    }

    let header = &normalized[0];
    out_lines.push(format!("| {} |", header.join(" | ")));
    out_lines.push(format!(
        "| {} |",
        std::iter::repeat("---").take(max_cols).collect::<Vec<_>>().join(" | ")
    ));
    for row in normalized.iter().skip(1) {
        out_lines.push(format!("| {} |", row.join(" | ")));
    }
    out_lines.push(String::new());
    stats.paragraphs += 1;
}

fn cell_has_complex_merge(tc: Node<'_, '_>) -> bool {
    for n in tc.descendants() {
        if n.tag_name().name() == "vMerge" {
            return true;
        }
        if n.tag_name().name() == "gridSpan" {
            let span = n
                .attributes()
                .find(|attr| attr.name() == "val" || attr.name() == "w:val")
                .map(|attr| attr.value().to_string())
                .unwrap_or_else(|| "1".to_string());
            if span.parse::<usize>().unwrap_or(1) > 1 {
                return true;
            }
        }
    }
    false
}

fn collect_ignored_features(doc: &Document<'_>, unsupported: &mut UnsupportedSummary) {
    for node in doc.descendants() {
        match node.tag_name().name() {
            "footnoteReference" | "endnoteReference" => unsupported.footnotes_ignored += 1,
            "oMath" | "oMathPara" => unsupported.equations_ignored += 1,
            _ => {}
        }
    }
}

fn paragraph_text(para: Node<'_, '_>) -> String {
    let mut text = String::new();
    for node in para.descendants() {
        match node.tag_name().name() {
            "t" => {
                if let Some(t) = node.text() {
                    text.push_str(t);
                }
            }
            "tab" => text.push('\t'),
            "br" => text.push('\n'),
            _ => {}
        }
    }
    text
}

fn heading_level(para: Node<'_, '_>) -> Option<usize> {
    let p_style_node = para
        .descendants()
        .find(|n| n.tag_name().name() == "pStyle")?;
    let p_style = p_style_node
        .attributes()
        .find(|attr| attr.name() == "val" || attr.name() == "w:val")
        .map(|attr| attr.value())?;
    if let Some(level_str) = p_style.strip_prefix("Heading") {
        if let Ok(level) = level_str.parse::<usize>() {
            if (1..=6).contains(&level) {
                return Some(level);
            }
        }
    }
    None
}

fn is_list_paragraph(para: Node<'_, '_>) -> bool {
    para.descendants().any(|n| n.tag_name().name() == "numPr")
}

fn default_history_file() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata)
            .join("docx2md_tui")
            .join("history.tsv");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("docx2md_tui_history.tsv")
}

fn default_config_file() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata)
            .join("docx2md_tui")
            .join("config.tsv");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("docx2md_tui_config.tsv")
}

fn load_history(path: &Path) -> AppResult<Vec<HistoryEntry>> {
    let content = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(map_io_error(
                e,
                &format!("读取历史文件失败: {}", path.display()),
            ))
        }
    };

    let mut items = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        items.push(HistoryEntry {
            input_path: parts[0].trim().to_string(),
            output_dir: parts[1].trim().to_string(),
            overwrite: parts.get(2).copied().unwrap_or("0") == "1",
            recursive: parts.get(3).copied().unwrap_or("0") == "1",
            keep_structure: parts.get(4).copied().unwrap_or("1") == "1",
        });
    }
    Ok(items)
}

fn save_history(path: &Path, entries: &[HistoryEntry]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                map_io_error(e, &format!("创建历史目录失败: {}", parent.display()))
            })?;
        }
    }
    let mut buf = String::new();
    for e in entries {
        let input = sanitize_field(&e.input_path);
        let output = sanitize_field(&e.output_dir);
        if input.is_empty() || output.is_empty() {
            continue;
        }
        buf.push_str(&input);
        buf.push('\t');
        buf.push_str(&output);
        buf.push('\t');
        buf.push_str(if e.overwrite { "1" } else { "0" });
        buf.push('\t');
        buf.push_str(if e.recursive { "1" } else { "0" });
        buf.push('\t');
        buf.push_str(if e.keep_structure { "1" } else { "0" });
        buf.push('\n');
    }
    fs::write(path, buf)
        .map_err(|e| map_io_error(e, &format!("写入历史失败: {}", path.display())))?;
    Ok(())
}

fn load_config(path: &Path) -> AppResult<AppConfig> {
    let content = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(e) => {
            return Err(map_io_error(
                e,
                &format!("读取配置文件失败: {}", path.display()),
            ))
        }
    };

    let mut map = HashMap::<String, String>::new();
    for line in content.lines() {
        let mut parts = line.splitn(2, '\t');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();
        if !key.is_empty() {
            map.insert(key.to_string(), val.to_string());
        }
    }

    Ok(AppConfig {
        last_input: map.get("last_input").cloned().unwrap_or_default(),
        last_output_dir: map.get("last_output_dir").cloned().unwrap_or_default(),
        options: ConvertOptions {
            overwrite: map.get("overwrite").map(|s| s == "1").unwrap_or(false),
            recursive: map.get("recursive").map(|s| s == "1").unwrap_or(false),
            keep_structure: map.get("keep_structure").map(|s| s == "1").unwrap_or(true),
        },
    })
}

fn save_config(path: &Path, config: &AppConfig) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                map_io_error(e, &format!("创建配置目录失败: {}", parent.display()))
            })?;
        }
    }
    let content = format!(
        "last_input\t{}\nlast_output_dir\t{}\noverwrite\t{}\nrecursive\t{}\nkeep_structure\t{}\n",
        sanitize_field(&config.last_input),
        sanitize_field(&config.last_output_dir),
        if config.options.overwrite { "1" } else { "0" },
        if config.options.recursive { "1" } else { "0" },
        if config.options.keep_structure { "1" } else { "0" }
    );
    fs::write(path, content)
        .map_err(|e| map_io_error(e, &format!("写入配置文件失败: {}", path.display())))?;
    Ok(())
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\t')
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_path_input(value: &str) -> String {
    value.trim().trim_matches('"').to_string()
}

fn is_docx_path(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().eq_ignore_ascii_case("docx"))
        .unwrap_or(false)
}

fn map_io_error(err: std::io::Error, context: &str) -> AppError {
    let kind = match err.kind() {
        std::io::ErrorKind::NotFound => ErrorKind::FileNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::IoFailure,
    };
    AppError::new(kind, format!("{context}: {err}"))
}

fn format_app_error(err: &AppError) -> String {
    let prefix = match err.kind {
        ErrorKind::FileNotFound => "文件不存在",
        ErrorKind::InvalidDocx => "无效 DOCX",
        ErrorKind::PermissionDenied => "权限不足",
        ErrorKind::ParseFailed => "解析失败",
        ErrorKind::IoFailure => "I/O 失败",
        ErrorKind::InvalidInput => "输入无效",
    };
    format!("{prefix}: {}", err.message)
}

fn bool_zh(v: bool) -> &'static str {
    if v {
        "开"
    } else {
        "关"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_paragraph_to_markdown() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>
                    <w:p>
                        <w:pPr><w:pStyle w:val="Heading2"/></w:pPr>
                        <w:r><w:t>标题</w:t></w:r>
                    </w:p>
                </w:body>
            </w:document>
        "#;
        let parsed = parse_document_xml_to_markdown(xml).unwrap();
        assert_eq!(parsed.markdown, "## 标题");
        assert_eq!(parsed.stats.headings, 1);
    }

    #[test]
    fn list_paragraph_to_markdown() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>
                    <w:p>
                        <w:pPr><w:numPr/></w:pPr>
                        <w:r><w:t>项1</w:t></w:r>
                    </w:p>
                </w:body>
            </w:document>
        "#;
        let parsed = parse_document_xml_to_markdown(xml).unwrap();
        assert_eq!(parsed.markdown, "- 项1");
        assert_eq!(parsed.stats.list_items, 1);
    }

    #[test]
    fn unsupported_features_detected() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>
                    <w:tbl>
                        <w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc></w:tr>
                    </w:tbl>
                    <w:p><w:r><w:drawing><a:blip xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" r:embed="rId9" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"/></w:drawing></w:r></w:p>
                    <w:p><w:r><w:footnoteReference w:id="1"/></w:r></w:p>
                    <m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"></m:oMath>
                </w:body>
            </w:document>
        "#;
        let parsed = parse_document_xml_to_markdown(xml).unwrap();
        assert_eq!(parsed.unsupported.tables_converted, 1);
        assert_eq!(parsed.unsupported.images_unresolved, 1);
        assert_eq!(parsed.unsupported.footnotes_ignored, 1);
        assert_eq!(parsed.unsupported.equations_ignored, 1);
    }

    #[test]
    fn table_to_markdown_basic() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>
                    <w:tbl>
                        <w:tr>
                            <w:tc><w:p><w:r><w:t>H1</w:t></w:r></w:p></w:tc>
                            <w:tc><w:p><w:r><w:t>H2</w:t></w:r></w:p></w:tc>
                        </w:tr>
                        <w:tr>
                            <w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc>
                            <w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc>
                        </w:tr>
                    </w:tbl>
                </w:body>
            </w:document>
        "#;
        let parsed = parse_document_xml_to_markdown(xml).unwrap();
        assert!(parsed.markdown.contains("| H1 | H2 |"));
        assert!(parsed.markdown.contains("| --- | --- |"));
        assert!(parsed.markdown.contains("| A | B |"));
        assert_eq!(parsed.unsupported.tables_converted, 1);
    }

    #[test]
    fn complex_table_degrades() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>
                    <w:tbl>
                        <w:tr>
                            <w:tc>
                                <w:tcPr><w:gridSpan w:val="2"/></w:tcPr>
                                <w:p><w:r><w:t>Merged</w:t></w:r></w:p>
                            </w:tc>
                        </w:tr>
                    </w:tbl>
                </w:body>
            </w:document>
        "#;
        let parsed = parse_document_xml_to_markdown(xml).unwrap();
        assert!(parsed.markdown.contains("该表格含合并单元格"));
        assert_eq!(parsed.unsupported.tables_degraded, 1);
    }
}
