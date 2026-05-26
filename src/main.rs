use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use arboard::Clipboard;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use roxmltree::{Document, Node};
use zip::ZipArchive;

const DOC_XML_PATH: &str = "word/document.xml";
const HISTORY_MAX: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    InputDialog,
    ConfirmList,
    HistoryList,
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    docx: String,
    output_md: String,
}

#[derive(Debug)]
struct App {
    stage: Stage,
    input_docx: String,
    output_preview: String,
    status: String,
    quit: bool,
    confirm_state: ListState,
    history_state: ListState,
    history_entries: Vec<HistoryEntry>,
    history_file: PathBuf,
}

impl App {
    fn new() -> Self {
        let history_file = default_history_file();

        let history_entries = match load_history(&history_file) {
            Ok(list) => list,
            Err(_) => Vec::new(),
        };

        let mut confirm_state = ListState::default();
        confirm_state.select(Some(0));

        let mut history_state = ListState::default();
        history_state.select(if history_entries.is_empty() { None } else { Some(0) });

        let status = if history_entries.is_empty() {
            "请粘贴或输入 DOCX 路径，回车进入确认（F2 可查看历史）".to_string()
        } else {
            format!(
                "已加载 {} 条历史记录，请输入 DOCX 路径（F2 可查看历史）",
                history_entries.len()
            )
        };

        Self {
            stage: Stage::InputDialog,
            input_docx: String::new(),
            output_preview: String::new(),
            status,
            quit: false,
            confirm_state,
            history_state,
            history_entries,
            history_file,
        }
    }
}

fn main() -> Result<()> {
    let mut app = App::new();
    run_tui(&mut app)
}

fn run_tui(app: &mut App) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;

    let mut stdout = std::io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let loop_result = event_loop(&mut terminal, app);

    disable_raw_mode().ok();
    let _ = terminal.backend_mut().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    loop_result
}

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| draw_ui(frame, app)).context("failed to draw ui")?;

        if event::poll(std::time::Duration::from_millis(200)).context("failed to poll event")? {
            match event::read().context("failed to read event")? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    handle_key_event(app, key.code, key.modifiers);
                }
                Event::Paste(text) => {
                    handle_paste(app, text);
                }
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
                    app.status = format!("读取剪贴板失败: {err:#}");
                }
                return;
            }
            _ => {}
        }
    }

    match app.stage {
        Stage::InputDialog => handle_input_dialog_keys(app, code),
        Stage::ConfirmList => handle_confirm_list_keys(app, code),
        Stage::HistoryList => handle_history_list_keys(app, code),
    }
}

fn handle_input_dialog_keys(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::F(2) => {
            if app.history_entries.is_empty() {
                app.status = "暂无历史记录".to_string();
            } else {
                app.stage = Stage::HistoryList;
                app.history_state.select(Some(0));
                app.status = "历史列表：回车回填，Delete 删除，Esc 返回".to_string();
            }
        }
        KeyCode::Backspace => {
            app.input_docx.pop();
            refresh_output_preview(app);
        }
        KeyCode::Enter => {
            if let Err(msg) = validate_input_docx(&app.input_docx) {
                app.status = msg;
                return;
            }

            match derive_output_md_path(&app.input_docx) {
                Ok(path) => {
                    app.output_preview = path;
                    app.stage = Stage::ConfirmList;
                    app.confirm_state.select(Some(0));
                    app.status = "请确认后回车开始转换".to_string();
                }
                Err(msg) => app.status = msg,
            }
        }
        KeyCode::Char(c) => {
            app.input_docx.push(c);
            refresh_output_preview(app);
        }
        _ => {}
    }
}

fn handle_confirm_list_keys(app: &mut App, code: KeyCode) {
    const CONFIRM_ITEMS: usize = 4;

    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            app.stage = Stage::InputDialog;
            app.status = "返回输入对话框".to_string();
        }
        KeyCode::Up => {
            let idx = app.confirm_state.selected().unwrap_or(0);
            let next = idx.saturating_sub(1);
            app.confirm_state.select(Some(next));
        }
        KeyCode::Down | KeyCode::Tab => {
            let idx = app.confirm_state.selected().unwrap_or(0);
            let next = if idx + 1 >= CONFIRM_ITEMS { 0 } else { idx + 1 };
            app.confirm_state.select(Some(next));
        }
        KeyCode::BackTab => {
            let idx = app.confirm_state.selected().unwrap_or(0);
            let next = if idx == 0 { CONFIRM_ITEMS - 1 } else { idx - 1 };
            app.confirm_state.select(Some(next));
        }
        KeyCode::Enter => {
            let selected = app.confirm_state.selected().unwrap_or(0);
            match selected {
                0 => {
                    let docx = app.input_docx.trim().to_string();
                    match derive_output_md_path(&docx) {
                        Ok(output_md) => match convert_docx_to_markdown(&docx, &output_md) {
                            Ok(lines) => {
                                app.output_preview = output_md.clone();
                                app.status = format!("转换成功，共输出 {lines} 行 -> {}", output_md);
                                if let Err(err) = remember_history(app, &docx, &output_md) {
                                    app.status = format!("{}；但保存历史失败: {err:#}", app.status);
                                }
                            }
                            Err(err) => {
                                app.status = format!("转换失败: {err:#}");
                            }
                        },
                        Err(msg) => app.status = msg,
                    }
                }
                1 => {
                    app.stage = Stage::InputDialog;
                    app.status = "返回输入对话框，请继续修改 DOCX 路径".to_string();
                }
                2 => {
                    if app.history_entries.is_empty() {
                        app.status = "暂无历史记录".to_string();
                    } else {
                        app.stage = Stage::HistoryList;
                        app.history_state.select(Some(0));
                        app.status = "历史列表：回车回填，Delete 删除，Esc 返回".to_string();
                    }
                }
                3 => app.quit = true,
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_history_list_keys(app: &mut App, code: KeyCode) {
    if app.history_entries.is_empty() {
        app.stage = Stage::InputDialog;
        app.status = "暂无历史记录，返回输入对话框".to_string();
        return;
    }

    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            app.stage = Stage::InputDialog;
            app.status = "已返回输入对话框".to_string();
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
            let next = (idx + 1) % app.history_entries.len();
            app.history_state.select(Some(next));
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
                    app.status = "历史记录已清空，返回输入对话框".to_string();
                } else {
                    let next = idx.min(app.history_entries.len() - 1);
                    app.history_state.select(Some(next));
                    app.status = "已删除该条历史记录".to_string();
                }

                if let Err(err) = save_history(&app.history_file, &app.history_entries) {
                    app.status = format!("保存历史失败: {err:#}");
                }
            }
        }
        KeyCode::Enter => {
            let idx = app.history_state.selected().unwrap_or(0);
            if let Some(entry) = app.history_entries.get(idx) {
                app.input_docx = entry.docx.clone();
                refresh_output_preview(app);
                app.stage = Stage::InputDialog;
                app.status = "已从历史记录回填路径，按 Enter 进入确认列表".to_string();
            }
        }
        _ => {}
    }
}

fn handle_paste(app: &mut App, text: String) {
    if app.stage != Stage::InputDialog {
        app.status = "请先回到输入对话框再粘贴路径".to_string();
        return;
    }

    let cleaned = text
        .trim_matches(|c| c == '\r' || c == '\n' || c == '"')
        .trim();
    if cleaned.is_empty() {
        app.status = "粘贴内容为空".to_string();
        return;
    }

    app.input_docx = cleaned.to_string();
    refresh_output_preview(app);
    app.status = "已粘贴 DOCX 路径".to_string();
}

fn paste_from_system_clipboard(app: &mut App) -> Result<()> {
    if app.stage != Stage::InputDialog {
        app.status = "请先回到输入对话框再粘贴路径".to_string();
        return Ok(());
    }

    let mut clipboard = Clipboard::new().context("无法访问系统剪贴板")?;
    let text = clipboard.get_text().context("剪贴板中没有可读文本")?;
    handle_paste(app, text);
    Ok(())
}

fn refresh_output_preview(app: &mut App) {
    app.output_preview = derive_output_md_path(&app.input_docx).unwrap_or_default();
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
        Stage::ConfirmList => draw_confirm_list(frame, root[1], app),
        Stage::HistoryList => draw_history_list(frame, root[1], app),
    }

    let footer_lines = vec![
        Line::from("快捷键: Ctrl+V 粘贴 | F2 历史 | Enter 确认 | q/Esc 退出"),
        Line::from(app.status.as_str()),
    ];

    let status = Paragraph::new(footer_lines)
        .block(Block::default().borders(Borders::ALL).title("状态"));
    frame.render_widget(status, root[2]);
}

fn draw_input_dialog(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(88, 75, area);
    frame.render_widget(Clear, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(1),
        ])
        .margin(1)
        .split(popup);

    let dialog_block = Block::default().borders(Borders::ALL).title("DOCX 输入对话框");
    frame.render_widget(dialog_block, popup);

    let docx = Paragraph::new(app.input_docx.as_str())
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title("DOCX 文件路径"));
    frame.render_widget(docx, chunks[0]);

    let preview = if app.output_preview.is_empty() {
        "（将自动输出同目录同名 .md）".to_string()
    } else {
        app.output_preview.clone()
    };

    let output = Paragraph::new(preview)
        .block(Block::default().borders(Borders::ALL).title("自动输出路径"));
    frame.render_widget(output, chunks[1]);

    let help = Paragraph::new(vec![
        Line::from("可直接手输，或 Ctrl+V 粘贴 DOCX 路径"),
        Line::from("无需手动填输出路径，程序自动输出为同目录同名 .md"),
        Line::from("按 Enter 进入确认列表，F2 打开历史路径"),
    ])
    .block(Block::default().borders(Borders::ALL).title("说明"));
    frame.render_widget(help, chunks[2]);
}

fn draw_confirm_list(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(88, 75, area);
    frame.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL).title("确认列表");
    frame.render_widget(block, popup);

    let output = if app.output_preview.is_empty() {
        "（将自动输出同目录同名 .md）".to_string()
    } else {
        app.output_preview.clone()
    };

    let items = vec![
        ListItem::new(format!("开始转换 -> {}", output)),
        ListItem::new("返回上一步修改 DOCX 路径"),
        ListItem::new("从历史记录选择路径"),
        ListItem::new("退出"),
    ];

    let detail = vec![
        Line::from(format!("DOCX: {}", app.input_docx)),
        Line::from(format!("输出: {}", output)),
        Line::from("回车执行当前选项"),
    ];

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(3)])
        .margin(1)
        .split(popup);

    let detail_para = Paragraph::new(detail)
        .block(Block::default().borders(Borders::ALL).title("路径确认"));
    frame.render_widget(detail_para, inner[0]);

    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = app.confirm_state.clone();
    frame.render_stateful_widget(list, inner[1], &mut state);
}

fn draw_history_list(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(92, 80, area);
    frame.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL).title("历史路径");
    frame.render_widget(block, popup);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .margin(1)
        .split(popup);

    let hint = Paragraph::new("回车回填当前路径 | Delete 删除一条 | Esc 返回")
        .block(Block::default().borders(Borders::ALL).title("操作"));
    frame.render_widget(hint, inner[0]);

    let items: Vec<ListItem> = app
        .history_entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let text = format!("{}. {}  =>  {}", idx + 1, entry.docx, entry.output_md);
            ListItem::new(text)
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = app.history_state.clone();
    frame.render_stateful_widget(list, inner[1], &mut state);
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

fn validate_input_docx(input_docx: &str) -> std::result::Result<(), String> {
    if input_docx.trim().is_empty() {
        return Err("DOCX 路径不能为空".to_string());
    }
    Ok(())
}

fn derive_output_md_path(input_docx: &str) -> std::result::Result<String, String> {
    let trimmed = input_docx.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err("DOCX 路径不能为空".to_string());
    }

    let path = PathBuf::from(trimmed);
    if path.file_name().is_none() {
        return Err("DOCX 路径无效，请输入具体文件路径".to_string());
    }

    let mut out = path;
    out.set_extension("md");

    out.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "输出路径包含无法识别的字符".to_string())
}

fn convert_docx_to_markdown(input_docx: &str, output_md: &str) -> Result<usize> {
    if input_docx.trim().is_empty() {
        return Err(anyhow!("DOCX 路径不能为空"));
    }
    if output_md.trim().is_empty() {
        return Err(anyhow!("输出路径不能为空"));
    }

    let docx_path = Path::new(input_docx.trim().trim_matches('"'));
    if !docx_path.exists() {
        return Err(anyhow!("DOCX 文件不存在: {}", docx_path.display()));
    }

    let markdown = extract_markdown_from_docx(docx_path)?;

    if let Some(parent) = Path::new(output_md).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create output parent directory: {}", parent.display())
            })?;
        }
    }

    fs::write(output_md, markdown.as_bytes())
        .with_context(|| format!("failed to write markdown file: {output_md}"))?;

    let lines = markdown.lines().count();
    Ok(lines)
}

fn extract_markdown_from_docx(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("failed to open docx: {}", path.display()))?;
    let mut archive = ZipArchive::new(file).context("failed to open zip archive from docx")?;

    let mut xml = String::new();
    {
        let mut doc_xml = archive
            .by_name(DOC_XML_PATH)
            .with_context(|| format!("docx missing {DOC_XML_PATH}"))?;
        doc_xml
            .read_to_string(&mut xml)
            .context("failed to read document.xml")?;
    }

    parse_document_xml_to_markdown(&xml)
}

fn parse_document_xml_to_markdown(xml: &str) -> Result<String> {
    let doc = Document::parse(xml).context("failed to parse document xml")?;

    let mut out_lines: Vec<String> = Vec::new();

    for para in doc.descendants().filter(|n| n.tag_name().name() == "p") {
        let text = paragraph_text(para);
        if text.trim().is_empty() {
            out_lines.push(String::new());
            continue;
        }

        if let Some(level) = heading_level(para) {
            let heading = format!("{} {}", "#".repeat(level), text.trim());
            out_lines.push(heading);
            out_lines.push(String::new());
            continue;
        }

        if is_list_paragraph(para) {
            out_lines.push(format!("- {}", text.trim()));
        } else {
            out_lines.push(text.trim().to_string());
            out_lines.push(String::new());
        }
    }

    while out_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        out_lines.pop();
    }

    Ok(out_lines.join("\n"))
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
    let p_style = para
        .descendants()
        .find(|n| n.tag_name().name() == "pStyle")
        .and_then(|n| n.attribute("w:val").or_else(|| n.attribute("val")))?;

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

fn load_history(path: &Path) -> Result<Vec<HistoryEntry>> {
    let content = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("读取历史文件失败: {}", path.display())),
    };

    let mut items = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let mut parts = line.splitn(2, '\t');
        let docx_raw = parts.next().unwrap_or("").trim();
        if docx_raw.is_empty() {
            continue;
        }

        let output_raw = parts.next().unwrap_or("").trim();
        let output_md = if output_raw.is_empty() {
            derive_output_md_path(docx_raw).unwrap_or_default()
        } else {
            output_raw.to_string()
        };

        items.push(HistoryEntry {
            docx: docx_raw.to_string(),
            output_md,
        });
    }

    Ok(items)
}

fn save_history(path: &Path, entries: &[HistoryEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("创建历史目录失败: {}", parent.display())
            })?;
        }
    }

    let mut buf = String::new();
    for item in entries {
        let docx = sanitize_history_field(&item.docx);
        let md = sanitize_history_field(&item.output_md);
        if docx.is_empty() || md.is_empty() {
            continue;
        }
        buf.push_str(&docx);
        buf.push('\t');
        buf.push_str(&md);
        buf.push('\n');
    }

    fs::write(path, buf).with_context(|| format!("写入历史文件失败: {}", path.display()))?;
    Ok(())
}

fn sanitize_history_field(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\t')
        .collect::<String>()
        .trim()
        .to_string()
}

fn remember_history(app: &mut App, docx: &str, output_md: &str) -> Result<()> {
    let docx = sanitize_history_field(docx);
    let output_md = sanitize_history_field(output_md);
    if docx.is_empty() || output_md.is_empty() {
        return Ok(());
    }

    app.history_entries.retain(|item| {
        !(item.docx.eq_ignore_ascii_case(&docx) && item.output_md.eq_ignore_ascii_case(&output_md))
    });

    app.history_entries.insert(0, HistoryEntry { docx, output_md });

    if app.history_entries.len() > HISTORY_MAX {
        app.history_entries.truncate(HISTORY_MAX);
    }

    app.history_state.select(Some(0));
    save_history(&app.history_file, &app.history_entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_paragraph_to_markdown() {
        let xml = r#"
            <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">
                <w:body>
                    <w:p>
                        <w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr>
                        <w:r><w:t>标题</w:t></w:r>
                    </w:p>
                </w:body>
            </w:document>
        "#;

        let md = parse_document_xml_to_markdown(xml).unwrap();
        assert_eq!(md, "## 标题");
    }

    #[test]
    fn list_paragraph_to_markdown() {
        let xml = r#"
            <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">
                <w:body>
                    <w:p>
                        <w:pPr><w:numPr/></w:pPr>
                        <w:r><w:t>项1</w:t></w:r>
                    </w:p>
                </w:body>
            </w:document>
        "#;

        let md = parse_document_xml_to_markdown(xml).unwrap();
        assert_eq!(md, "- 项1");
    }
}
