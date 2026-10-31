//! Interactive terminal UI: paste a Teamcraft link to plan it, then click market items to copy
//! their names (for pasting into the in-game market board search) and tick them off.

use std::collections::HashSet;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::cache::Http;
use crate::pipeline::{self, Inputs, Options};
use crate::view::{View, gil};

const TABS: [&str; 4] = ["Market", "Route", "Crafts", "Summary"];
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
/// Windows consoles deliver a paste as a burst of key presses rather than a paste event.
/// A burst at least this long is treated as pasted text.
const PASTE_BURST: usize = 8;

pub fn run(opts: Options, refresh: bool, list: Option<String>, watch_clipboard: bool) -> Result<()> {
    let mut terminal = ratatui::init();
    // Bracketed paste isn't available on every platform; the burst heuristic covers the rest.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let mut opts = opts;
    if opts.market.is_empty() {
        opts.market = saved_market().unwrap_or_default();
    }
    let mut app = App::new(opts, refresh, watch_clipboard);
    if app.opts.market.is_empty() {
        app.open_prompt(PromptKind::Market);
    }
    if let Some(list) = list {
        app.start_plan(Source::Teamcraft(list));
    }
    let result = app.event_loop(&mut terminal);

    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

enum Msg {
    Progress(String),
    Done(Result<Box<View>, String>),
}

enum Row {
    Heading(usize),
    Line { group: usize, line: usize },
    Shortfall(usize),
    Text(String),
    Blank,
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Line { .. } | Row::Shortfall(_))
    }
}

/// What a plan is for: a Teamcraft list, or items typed in the TUI.
#[derive(Clone, PartialEq, Debug)]
enum Source {
    Teamcraft(String),
    Items(Vec<String>),
}

impl Source {
    fn inputs(&self) -> Inputs {
        match self {
            Source::Teamcraft(list) => Inputs { teamcraft: Some(list.clone()), ..Default::default() },
            Source::Items(items) => Inputs { items: items.clone(), ..Default::default() },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PromptKind {
    Market,
    Items,
}

impl PromptKind {
    fn title(self) -> &'static str {
        match self {
            PromptKind::Market => " Market: world, data center or region (e.g. Crystal, Coeurl, North-America) ",
            PromptKind::Items => " Items, comma-separated (e.g. 3x Iron Ingot, Cobalt Ingot:5, Hippogryph Leather) ",
        }
    }
}

struct Prompt {
    kind: PromptKind,
    buffer: String,
}

struct Loading {
    source: Source,
    progress: String,
    started: Instant,
    rx: Receiver<Msg>,
}

struct App {
    opts: Options,
    refresh: bool,
    view: Option<View>,
    loading: Option<Loading>,
    error: Option<String>,
    last_source: Option<Source>,
    prompt: Option<Prompt>,
    /// A plan requested before a market was chosen; runs once one is.
    pending: Option<Source>,

    tab: usize,
    rows: Vec<Row>,
    bought: HashSet<usize>,
    market_state: ListState,
    scroll: u16,

    message: Option<(String, Instant)>,
    clipboard: Option<arboard::Clipboard>,
    watch_clipboard: bool,
    last_clip: Option<String>,
    last_clip_check: Instant,

    // Screen geometry from the last draw, for mouse hit-testing.
    market_inner: Rect,
    tab_hits: Vec<(u16, u16, u16)>,
    quit: bool,
}

impl App {
    fn new(opts: Options, refresh: bool, watch_clipboard: bool) -> Self {
        Self {
            opts,
            refresh,
            view: None,
            loading: None,
            error: None,
            last_source: None,
            prompt: None,
            pending: None,
            tab: 0,
            rows: Vec::new(),
            bought: HashSet::new(),
            market_state: ListState::default(),
            scroll: 0,
            message: None,
            clipboard: arboard::Clipboard::new().ok(),
            watch_clipboard,
            last_clip: None,
            last_clip_check: Instant::now(),
            market_inner: Rect::default(),
            tab_hits: Vec::new(),
            quit: false,
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                let mut batch = vec![event::read()?];
                while event::poll(Duration::ZERO)? {
                    batch.push(event::read()?);
                }
                self.handle_batch(batch);
            }
            self.poll_planner();
            self.poll_clipboard();
        }
        Ok(())
    }

    fn handle_batch(&mut self, batch: Vec<Event>) {
        let typed: String = batch
            .iter()
            .filter_map(|e| match e {
                Event::Key(KeyEvent { code: KeyCode::Char(c), kind: KeyEventKind::Press, modifiers, .. })
                    if !modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    Some(*c)
                }
                _ => None,
            })
            .collect();
        let burst = typed.chars().count() >= PASTE_BURST;
        if self.prompt.is_some() {
            for event in batch {
                match event {
                    Event::Paste(text) => self.prompt_insert(&text),
                    // A pasted multi-line list arrives with Enter between lines; don't submit on those.
                    Event::Key(KeyEvent { code: KeyCode::Enter, kind: KeyEventKind::Press, .. }) if burst => {
                        self.prompt_separator()
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.prompt_key(key),
                    _ => {}
                }
            }
            return;
        }
        if burst {
            self.handle_paste(&typed);
            return;
        }
        for event in batch {
            match event {
                Event::Paste(text) => self.handle_paste(&text),
                Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                Event::Mouse(mouse) => self.handle_mouse(mouse),
                _ => {}
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        match teamcraft_link(text) {
            Some(link) => self.start_plan(Source::Teamcraft(link)),
            None => self.flash("That doesn't look like a Teamcraft list link (press i to type items instead)"),
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let buffer = match (kind, &self.last_source) {
            (PromptKind::Market, _) => self.opts.market.clone(),
            (PromptKind::Items, Some(Source::Items(items))) => items.join(", "),
            (PromptKind::Items, _) => String::new(),
        };
        self.prompt = Some(Prompt { kind, buffer });
    }

    fn prompt_insert(&mut self, text: &str) {
        let Some(prompt) = &mut self.prompt else { return };
        match prompt.kind {
            PromptKind::Market => prompt.buffer.push_str(text.lines().next().unwrap_or_default().trim()),
            // One item per line is a natural way to paste a list; turn lines into entries.
            PromptKind::Items => {
                let joined = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(", ");
                if !prompt.buffer.trim().is_empty() && !joined.is_empty() && !prompt.buffer.trim_end().ends_with(',') {
                    prompt.buffer.push_str(", ");
                }
                prompt.buffer.push_str(&joined);
            }
        }
    }

    /// Separates entries in the items prompt (used for line breaks inside a paste).
    fn prompt_separator(&mut self) {
        if let Some(prompt) = &mut self.prompt
            && prompt.kind == PromptKind::Items
            && !prompt.buffer.trim().is_empty()
            && !prompt.buffer.trim_end().ends_with(',')
        {
            prompt.buffer.push_str(", ");
        }
    }

    fn prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(prompt) = &mut self.prompt else { return };
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
                self.pending = None;
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Backspace => {
                prompt.buffer.pop();
            }
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('u') if ctrl => prompt.buffer.clear(),
            KeyCode::Char('v') if ctrl => {
                if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                    self.prompt_insert(&text);
                }
            }
            KeyCode::Char(c) if !ctrl => prompt.buffer.push(c),
            _ => {}
        }
    }

    fn submit_prompt(&mut self) {
        let Some(prompt) = self.prompt.take() else { return };
        let text = prompt.buffer.trim().to_string();
        match prompt.kind {
            PromptKind::Market => {
                if text.is_empty() {
                    self.flash("Enter a world, data center or region");
                    self.prompt = Some(prompt);
                    return;
                }
                let changed = !self.opts.market.eq_ignore_ascii_case(&text);
                self.opts.market = text;
                save_market(&self.opts.market);
                // Re-plan whatever was waiting on a market, or the current plan at the new market.
                match self.pending.take() {
                    Some(source) => self.start_plan(source),
                    None => match self.last_source.clone() {
                        Some(source) if changed => self.start_plan(source),
                        _ => self.flash(&format!("Market set to {}", self.opts.market)),
                    },
                }
            }
            PromptKind::Items => {
                let items: Vec<String> = text
                    .split([',', ';', '\n'])
                    .map(str::trim)
                    .filter(|s| crate::input::parse_entry(s).is_some())
                    .map(str::to_string)
                    .collect();
                if items.is_empty() {
                    self.flash("No items entered");
                    self.prompt = Some(prompt);
                    return;
                }
                self.start_plan(Source::Items(items));
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('v') if ctrl => self.import_from_clipboard(),
            KeyCode::Char('p') => self.import_from_clipboard(),
            KeyCode::Char('m') => self.open_prompt(PromptKind::Market),
            KeyCode::Char('i') => self.open_prompt(PromptKind::Items),
            KeyCode::Char('r') => match self.last_source.clone() {
                Some(source) => self.start_plan(source),
                None => self.flash("Nothing to reload yet"),
            },
            KeyCode::Tab | KeyCode::Right => self.set_tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab | KeyCode::Left => self.set_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Char(c @ '1'..='4') => self.set_tab(c as usize - '1' as usize),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(i) = self.market_state.selected() {
                    self.activate(i);
                }
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, _, tab)) =
                    self.tab_hits.iter().find(|(x, w, _)| mouse.column >= *x && mouse.column < x + w)
                    && mouse.row == self.tab_row()
                {
                    self.set_tab(tab as usize);
                    return;
                }
                let area = self.market_inner;
                let inside = mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height;
                if self.tab == 0 && inside {
                    let index = self.market_state.offset() + (mouse.row - area.y) as usize;
                    if self.rows.get(index).is_some_and(Row::selectable) {
                        self.market_state.select(Some(index));
                        self.activate(index);
                    }
                }
            }
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::ScrollUp => self.move_by(-3),
            _ => {}
        }
    }

    fn tab_row(&self) -> u16 {
        // The tab bar sits directly below the 5-line header.
        5
    }

    fn set_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.scroll = 0;
    }

    fn move_by(&mut self, delta: i32) {
        if self.tab != 0 {
            self.scroll = (self.scroll as i32 + delta).max(0) as u16;
            return;
        }
        let selectable: Vec<usize> = (0..self.rows.len()).filter(|i| self.rows[*i].selectable()).collect();
        if selectable.is_empty() {
            return;
        }
        let current = self.market_state.selected().and_then(|s| selectable.iter().position(|i| *i == s)).unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, selectable.len() as i32 - 1) as usize;
        self.market_state.select(Some(selectable[next]));
    }

    /// Copies the row's item name and toggles its bought marker.
    fn activate(&mut self, index: usize) {
        let Some(view) = &self.view else { return };
        let name = match self.rows.get(index) {
            Some(Row::Line { group, line }) => view.market[*group].lines[*line].item.clone(),
            Some(Row::Shortfall(i)) => view.shortfalls[*i].item.clone(),
            _ => return,
        };
        let copied = self.copy(&name);
        let marked = if self.bought.remove(&index) {
            "unmarked"
        } else {
            self.bought.insert(index);
            "marked bought"
        };
        let copy_note = if copied { format!("Copied \"{name}\"") } else { format!("Couldn't copy \"{name}\"") };
        self.flash(&format!("{copy_note} · {marked}"));
    }

    fn copy(&mut self, text: &str) -> bool {
        if let Some(cb) = &mut self.clipboard
            && cb.set_text(text.to_owned()).is_ok()
        {
            self.last_clip = Some(text.to_owned());
            return true;
        }
        // Fall back to asking the terminal to set the clipboard (OSC 52); works over SSH too.
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut out = std::io::stdout();
        write!(out, "\x1b]52;c;{encoded}\x07").and_then(|_| out.flush()).is_ok()
    }

    fn import_from_clipboard(&mut self) {
        match self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
            Some(text) => self.handle_paste(&text),
            None => self.flash("Couldn't read the clipboard"),
        }
    }

    fn poll_clipboard(&mut self) {
        if !self.watch_clipboard
            || self.loading.is_some()
            || self.prompt.is_some()
            || self.last_clip_check.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        self.last_clip_check = Instant::now();
        let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) else { return };
        if self.last_clip.as_deref() == Some(text.as_str()) {
            return;
        }
        self.last_clip = Some(text.clone());
        if let Some(link) = teamcraft_link(&text) {
            let source = Source::Teamcraft(link);
            if self.last_source.as_ref() != Some(&source) {
                self.start_plan(source);
            }
        }
    }

    fn start_plan(&mut self, source: Source) {
        if self.loading.is_some() {
            self.flash("Still planning the previous list");
            return;
        }
        if self.opts.market.is_empty() {
            self.pending = Some(source);
            self.open_prompt(PromptKind::Market);
            self.flash("Choose a market first");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let opts = self.opts.clone();
        let refresh = std::mem::take(&mut self.refresh);
        let inputs = source.inputs();
        let progress_text = match &source {
            Source::Teamcraft(_) => "Loading Teamcraft list...",
            Source::Items(_) => "Looking up items...",
        };
        thread::spawn(move || {
            let progress = tx.clone();
            let result = Http::new(refresh, move |msg| {
                let _ = progress.send(Msg::Progress(msg.to_string()));
            })
            .and_then(|http| pipeline::run(&http, &opts, &inputs));
            let _ = tx.send(Msg::Done(result.map(Box::new).map_err(|e| format!("{e:#}"))));
        });
        self.loading = Some(Loading { source, progress: progress_text.into(), started: Instant::now(), rx });
        self.error = None;
    }

    fn poll_planner(&mut self) {
        let Some(loading) = &mut self.loading else { return };
        loop {
            match loading.rx.try_recv() {
                Ok(Msg::Progress(p)) => loading.progress = p,
                Ok(Msg::Done(result)) => {
                    let source = loading.source.clone();
                    self.loading = None;
                    // Remember failed sources too, so r retries them.
                    self.last_source = Some(source);
                    match result {
                        Ok(view) => {
                            self.set_view(*view);
                            self.flash("Plan ready · click an item (or Enter) to copy its name and tick it off");
                        }
                        Err(e) => self.error = Some(e),
                    }
                    return;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.loading = None;
                    self.error = Some("planning stopped unexpectedly".into());
                    return;
                }
            }
        }
    }

    fn set_view(&mut self, view: View) {
        let mut rows = Vec::new();
        for (g, group) in view.market.iter().enumerate() {
            rows.push(Row::Heading(g));
            rows.extend((0..group.lines.len()).map(|line| Row::Line { group: g, line }));
            rows.push(Row::Blank);
        }
        if !view.overbuys.is_empty() {
            rows.push(Row::Text(format!("Stacks can't be split, so you'll have extra: {}", view.overbuys.join(", "))));
            rows.push(Row::Blank);
        }
        if !view.shortfalls.is_empty() {
            rows.push(Row::Text("Not enough on the market (buy what's there, find the rest):".into()));
            rows.extend((0..view.shortfalls.len()).map(Row::Shortfall));
        }
        if view.market.is_empty() && view.shortfalls.is_empty() {
            rows.push(Row::Text("Nothing to buy on the market board.".into()));
        }
        self.rows = rows;
        self.bought.clear();
        self.market_state = ListState::default();
        self.view = Some(view);
        self.move_by(0);
        self.scroll = 0;
    }

    fn flash(&mut self, msg: &str) {
        self.message = Some((msg.to_string(), Instant::now()));
    }

    // ----- drawing -----

    fn draw(&mut self, f: &mut Frame) {
        let [header, tabs, body, footer] =
            Layout::vertical([Constraint::Length(5), Constraint::Length(1), Constraint::Min(3), Constraint::Length(2)])
                .areas(f.area());
        self.draw_header(f, header);
        self.draw_tabs(f, tabs);
        match &self.view {
            Some(_) => match self.tab {
                0 => self.draw_market(f, body),
                1 => self.draw_text(f, body, route_lines(self.view.as_ref().unwrap())),
                2 => self.draw_text(f, body, craft_lines(self.view.as_ref().unwrap())),
                _ => self.draw_text(f, body, summary_lines(self.view.as_ref().unwrap())),
            },
            None => self.draw_placeholder(f, body),
        }
        self.draw_footer(f, footer);
        self.draw_prompt(f);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" cheapster ");
        let lines = match &self.view {
            Some(v) => {
                let total: u64 = v.market.iter().map(|g| g.subtotal()).sum();
                let left: u64 = self.remaining_cost();
                vec![
                    Line::from(v.title.clone().bold()),
                    Line::from(vec![
                        Span::raw(format!("{} {} ", v.total, v.breakdown)),
                        Span::styled(
                            format!("· market left to buy: {} of {} gil", gil(left), gil(total)),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]),
                    Line::from(v.context.clone().dim()),
                ]
            }
            None => vec![
                Line::from("No plan yet.".bold()),
                Line::from(
                    format!(
                        "Market: {} · Start: {}",
                        if self.opts.market.is_empty() { "not set (press m)" } else { &self.opts.market },
                        self.opts.start
                    )
                    .dim(),
                ),
            ],
        };
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_tabs(&mut self, f: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        let mut x = area.x;
        self.tab_hits.clear();
        for (i, title) in TABS.iter().enumerate() {
            let label = format!(" {} {title} ", i + 1);
            let width = label.chars().count() as u16;
            let style = if i == self.tab {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            self.tab_hits.push((x, width, i as u16));
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
            x += width + 1;
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_market(&mut self, f: &mut Frame, area: Rect) {
        let view = self.view.as_ref().unwrap();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                Row::Heading(g) => {
                    let group = &view.market[*g];
                    let left: u64 = self
                        .rows
                        .iter()
                        .enumerate()
                        .filter(|(j, r)| matches!(r, Row::Line { group, .. } if group == g) && !self.bought.contains(j))
                        .filter_map(|(_, r)| match r {
                            Row::Line { group, line } => Some(view.market[*group].lines[*line].cost),
                            _ => None,
                        })
                        .sum();
                    let style = if group.home {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(group.heading(), style),
                        Span::raw(format!("  {} gil", gil(group.subtotal()))),
                        Span::styled(
                            if left == group.subtotal() { String::new() } else { format!(" ({} left)", gil(left)) },
                            Style::default().fg(Color::Yellow),
                        ),
                    ]))
                }
                Row::Line { group, line } => {
                    let text = view.market[*group].lines[*line].describe();
                    checkbox_item(&text, self.bought.contains(&i))
                }
                Row::Shortfall(s) => {
                    let short = &view.shortfalls[*s];
                    let text = match &short.listings {
                        Some(l) => format!("{} — {l}", short.summary),
                        None => short.summary.clone(),
                    };
                    checkbox_item(&text, self.bought.contains(&i))
                }
                Row::Text(t) => ListItem::new(Line::from(t.clone().dim())),
                Row::Blank => ListItem::new(""),
            })
            .collect();
        let block = Block::default().borders(Borders::ALL).title(" Market board · click or Enter to copy & tick ");
        self.market_inner = block.inner(area);
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.market_state);
    }

    fn draw_text(&self, f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
        let block = Block::default().borders(Borders::ALL).title(format!(" {} ", TABS[self.tab]));
        f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), area);
    }

    fn draw_placeholder(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from("")];
        if let Some(loading) = &self.loading {
            let frame = SPINNER[(loading.started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
            lines.push(Line::from(format!("{frame} {}", loading.progress).bold()));
        } else if let Some(err) = &self.error {
            lines.push(Line::from("Couldn't make a plan:".red().bold()));
            lines.push(Line::from(err.clone().red()));
            lines.push(Line::from(""));
            lines.push(Line::from("Press r to retry, m to change the market, i to edit items, or paste another link."));
        } else {
            if self.opts.market.is_empty() {
                lines.push(Line::from("First, press m to choose a market (world, data center or region).".yellow()));
                lines.push(Line::from(""));
            }
            lines.push(Line::from("Copy a Teamcraft list link, then paste it here (Ctrl+V) or press p.".bold()));
            lines.push(Line::from("Or press i to type the items you want to make.".bold()));
            if self.watch_clipboard {
                lines.push(Line::from("Watching the clipboard: just copying a link is enough.".dim()));
            }
        }
        let block = Block::default().borders(Borders::ALL);
        f.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(block),
            area,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let [status, help] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        let status_line = if let Some(loading) = &self.loading {
            let frame = SPINNER[(loading.started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
            Line::from(format!("{frame} {}", loading.progress).yellow())
        } else if let (Some(err), Some(_)) = (&self.error, &self.view) {
            Line::from(format!("Error: {err}").red())
        } else if let Some((msg, at)) = &self.message
            && at.elapsed() < Duration::from_secs(6)
        {
            Line::from(msg.clone().green())
        } else {
            Line::from("")
        };
        f.render_widget(Paragraph::new(status_line), status);
        let keys = if self.prompt.is_some() {
            "Enter plan · Esc cancel · Ctrl+V paste · Ctrl+U clear"
        } else {
            "p/Ctrl+V paste link · i items · m market · click/Enter copy+tick · Tab views · r reload · q quit"
        };
        f.render_widget(Paragraph::new(Line::from(keys.dim())), help);
    }

    fn draw_prompt(&self, f: &mut Frame) {
        let Some(prompt) = &self.prompt else { return };
        let area = f.area();
        let width = area.width.saturating_sub(4).min(100);
        let popup = Rect { x: area.x + (area.width - width) / 2, y: area.y + area.height / 3, width, height: 5 };
        let hint = match prompt.kind {
            PromptKind::Market => "Prices come from this market; world visits within a data center are free.",
            PromptKind::Items => "Pasting one item per line works too.",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(prompt.kind.title());
        // Keep the end of long input visible.
        let inner_width = width.saturating_sub(4) as usize;
        let chars: Vec<char> = prompt.buffer.chars().collect();
        let visible: String = chars[chars.len().saturating_sub(inner_width)..].iter().collect();
        let lines = vec![
            Line::from(vec![Span::raw(format!(" {visible}")), Span::styled("▏", Style::default().fg(Color::Cyan))]),
            Line::from(""),
            Line::from(format!(" {hint}").dim()),
        ];
        f.render_widget(Clear, popup);
        f.render_widget(Paragraph::new(lines).block(block), popup);
    }

    fn remaining_cost(&self) -> u64 {
        let Some(view) = &self.view else { return 0 };
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.bought.contains(i))
            .filter_map(|(_, r)| match r {
                Row::Line { group, line } => Some(view.market[*group].lines[*line].cost),
                _ => None,
            })
            .sum()
    }
}

fn checkbox_item(text: &str, bought: bool) -> ListItem<'static> {
    if bought {
        ListItem::new(Line::from(vec![
            Span::styled("  [x] ", Style::default().fg(Color::Green)),
            Span::styled(text.to_string(), Style::default().add_modifier(Modifier::CROSSED_OUT).fg(Color::DarkGray)),
        ]))
    } else {
        ListItem::new(Line::from(format!("  [ ] {text}")))
    }
}

fn route_lines(view: &View) -> Vec<Line<'static>> {
    if view.route.is_empty() {
        return vec![Line::from("No travel needed.")];
    }
    let mut lines = Vec::new();
    for stop in &view.route {
        lines.push(Line::from(stop.heading.clone().cyan().bold()));
        lines.extend(stop.errands.iter().map(|e| Line::from(format!("  • {e}"))));
        lines.push(Line::from(""));
    }
    lines
}

fn craft_lines(view: &View) -> Vec<Line<'static>> {
    if view.crafts.is_empty() {
        return vec![Line::from("Nothing to craft: the plan buys the finished items.")];
    }
    let mut lines = vec![Line::from("In this order:".dim())];
    lines.extend(view.crafts.iter().map(|c| Line::from(format!("  {c}"))));
    lines
}

fn summary_lines(view: &View) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(view.total.clone().bold()), Line::from(view.breakdown.clone()), Line::from("")];
    lines.push(Line::from("Buying the finished items on the market instead:".cyan().bold()));
    lines.extend(view.finished.iter().map(|l| Line::from(format!("  • {l}"))));
    if let Some(t) = &view.finished_total {
        lines.push(Line::from(format!("  {t}")));
    }
    if !view.on_hand.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Already on hand (marked obtained in Teamcraft):".cyan().bold()));
        lines.extend(view.on_hand.iter().map(|l| Line::from(format!("  • {l}"))));
    }
    for note in &view.notes {
        lines.push(Line::from(""));
        lines.push(Line::from(note.clone().dim()));
    }
    lines
}

/// Picks a Teamcraft list link (or bare list id) out of pasted text.
fn teamcraft_link(text: &str) -> Option<String> {
    let trim = |w: &str| w.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')')).to_string();
    if let Some(word) = text
        .split_whitespace()
        .find(|w| w.contains("ffxivteamcraft.com/list/") || w.contains("ffxivteamcraft.com/import/"))
    {
        return Some(trim(word));
    }
    let word = text.trim();
    let looks_like_id = word.len() >= 15 && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    looks_like_id.then(|| word.to_string())
}

fn settings_path() -> std::path::PathBuf {
    crate::cache::cache_dir().join("tui-settings.json")
}

/// The market chosen in a previous TUI session, if any.
fn saved_market() -> Option<String> {
    let text = std::fs::read_to_string(settings_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value["market"].as_str().filter(|m| !m.is_empty()).map(str::to_string)
}

fn save_market(market: &str) {
    // Best effort: losing this only means being asked again next time.
    let _ = std::fs::write(settings_path(), serde_json::json!({ "market": market }).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::view::{MarketLine, WorldGroup};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_view() -> View {
        let line = |item: &str, quantity, ppu| MarketLine {
            item: item.into(),
            quantity,
            hq: false,
            price_per_unit: ppu,
            cost: ppu * quantity as u64 * 105 / 100,
        };
        View {
            title: "Cheapest plan for: 10× Hippogryph Leather".into(),
            context: "Market: Crystal".into(),
            total: "Total: 950 gil".into(),
            breakdown: "(market 406)".into(),
            finished: vec![],
            finished_total: None,
            on_hand: vec![],
            route: vec![],
            market: vec![
                WorldGroup { world: "Diabolos".into(), home: false, lines: vec![line("Hippogryph Skin", 3, 49)] },
                WorldGroup { world: "Coeurl".into(), home: true, lines: vec![line("Black Alumen", 8, 30)] },
            ],
            overbuys: vec![],
            crafts: vec![],
            shortfalls: vec![],
            notes: vec![],
        }
    }

    fn test_app() -> App {
        let opts = Options {
            market: "Crystal".into(),
            home_world: Some("Coeurl".into()),
            start: "Limsa".into(),
            hq: false,
            no_gather: false,
            no_timed: false,
            gather_cost: 0,
            include_crystals: false,
        };
        let mut app = App::new(opts, false, false);
        // Never touch the real clipboard from tests.
        app.clipboard = None;
        app.set_view(sample_view());
        app
    }

    fn screen_row(term: &Terminal<TestBackend>, needle: &str) -> u16 {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .find(|y| (0..buf.area.width).map(|x| buf[(x, *y)].symbol()).collect::<String>().contains(needle))
            .unwrap_or_else(|| panic!("\"{needle}\" not on screen"))
    }

    fn click(app: &mut App, column: u16, row: u16) {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn clicking_a_market_line_ticks_it_and_reduces_what_is_left() {
        let mut app = test_app();
        let mut term = Terminal::new(TestBackend::new(110, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let before = app.remaining_cost();

        let row = screen_row(&term, "3× Hippogryph Skin");
        click(&mut app, 12, row);
        assert_eq!(app.bought.len(), 1);
        assert!(app.remaining_cost() < before);
        assert!(app.message.as_ref().unwrap().0.contains("Hippogryph Skin"));

        // Clicking a world heading does nothing; clicking the line again unticks it.
        term.draw(|f| app.draw(f)).unwrap();
        click(&mut app, 12, screen_row(&term, "Diabolos"));
        assert_eq!(app.bought.len(), 1);
        click(&mut app, 12, row);
        assert!(app.bought.is_empty());
    }

    #[test]
    fn home_world_is_listed_last_and_tabs_are_clickable() {
        let mut app = test_app();
        let mut term = Terminal::new(TestBackend::new(110, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(screen_row(&term, "Diabolos") < screen_row(&term, "Coeurl (home)"));

        let (x, _, _) = app.tab_hits[2];
        let row = app.tab_row();
        click(&mut app, x + 1, row);
        assert_eq!(app.tab, 2);
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_batch(vec![key(KeyCode::Char(c))]);
        }
    }

    #[test]
    fn typed_items_wait_for_a_market_before_planning() {
        let mut app = test_app();
        app.opts.market.clear();

        app.handle_batch(vec![key(KeyCode::Char('i'))]);
        assert_eq!(app.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Items));
        // Typed one key at a time, so 'q' and 'r' are text here, not shortcuts.
        type_text(&mut app, "3x Iron Ingot, Cobalt Ingot:5");
        assert!(!app.quit);

        // A multi-line paste arrives as a burst of keys with Enter between lines.
        app.prompt.as_mut().unwrap().buffer.clear();
        let mut burst: Vec<Event> = "Mythril Ingot".chars().map(|c| key(KeyCode::Char(c))).collect();
        burst.push(key(KeyCode::Enter));
        burst.extend("Hard Leather".chars().map(|c| key(KeyCode::Char(c))));
        app.handle_batch(burst);
        assert_eq!(app.prompt.as_ref().unwrap().buffer, "Mythril Ingot, Hard Leather");

        app.prompt.as_mut().unwrap().buffer = "3x Iron Ingot, Cobalt Ingot:5\nHard Leather".into();
        app.handle_batch(vec![key(KeyCode::Enter)]);
        // No market yet: the plan is queued and the market prompt opens instead of a network call.
        assert!(app.loading.is_none());
        assert_eq!(app.prompt.as_ref().map(|p| p.kind), Some(PromptKind::Market));
        assert_eq!(
            app.pending,
            Some(Source::Items(vec!["3x Iron Ingot".into(), "Cobalt Ingot:5".into(), "Hard Leather".into()]))
        );

        // Cancelling the market prompt drops the queued plan.
        app.handle_batch(vec![key(KeyCode::Esc)]);
        assert!(app.prompt.is_none() && app.pending.is_none());
    }

    #[test]
    fn pasting_lines_into_the_items_prompt_makes_entries() {
        let mut app = test_app();
        app.open_prompt(PromptKind::Items);
        app.prompt.as_mut().unwrap().buffer = "Iron Ingot".into();
        app.prompt_insert("Cobalt Ingot\n\n2x Mythril Ingot\n");
        assert_eq!(app.prompt.as_ref().unwrap().buffer, "Iron Ingot, Cobalt Ingot, 2x Mythril Ingot");
    }

    #[test]
    fn finds_teamcraft_links() {
        assert_eq!(
            teamcraft_link("check this <https://ffxivteamcraft.com/list/abc123> out").as_deref(),
            Some("https://ffxivteamcraft.com/list/abc123")
        );
        assert_eq!(teamcraft_link("GeloHs9UZ1PyHHnl1QD2").as_deref(), Some("GeloHs9UZ1PyHHnl1QD2"));
        assert_eq!(teamcraft_link("Hippogryph Skin"), None);
    }
}
