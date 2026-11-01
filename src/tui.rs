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
use crate::config::{self, Config, Crafter, GATHERERS, Gatherer, JOBS};
use crate::pipeline::{self, Inputs, Options};
use crate::universalis::{self, Region};
use crate::view::{View, gil};

const TABS: [&str; 4] = ["Market", "Route", "Crafts", "Summary"];
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
/// Windows consoles deliver a paste as a burst of key presses rather than a paste event.
/// A burst at least this long is treated as pasted text.
const PASTE_BURST: usize = 8;
/// Keys arriving closer together than this are part of the same burst.
const PASTE_GAP: Duration = Duration::from_millis(15);

/// Runs the TUI, planning `start` right away if given.
pub fn run(opts: Options, refresh: bool, start: Option<Source>, watch_clipboard: bool) -> Result<()> {
    let mut terminal = ratatui::init();
    // On Windows, crossterm reads console key events and can't decode bracketed paste: turning it
    // on makes the terminal send its ESC[200~ marker as an Esc key press. The burst heuristic
    // handles pastes there instead.
    if cfg!(not(windows)) {
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    }
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let mut app = App::new(opts, refresh, watch_clipboard);
    app.persist = true;
    app.load_markets();
    if app.opts.market.is_empty() {
        app.open_market_picker();
    }
    if let Some(source) = start {
        app.start_plan(source);
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
    Line {
        group: usize,
        line: usize,
    },
    Shortfall(usize),
    /// A craft step's summary line.
    Craft(usize),
    Macro {
        step: usize,
        block: usize,
    },
    Text(String),
    Blank,
}

impl Row {
    fn selectable(&self) -> bool {
        matches!(self, Row::Line { .. } | Row::Shortfall(_) | Row::Macro { .. })
    }
}

/// A clickable list (market purchases, or crafts and their macros) with tick marks.
#[derive(Default)]
struct Pane {
    rows: Vec<Row>,
    state: ListState,
    ticked: HashSet<usize>,
    /// Where the list was last drawn, for mouse hit-testing.
    inner: Rect,
}

impl Pane {
    fn reset(&mut self, rows: Vec<Row>) {
        *self = Pane { rows, ..Pane::default() };
        self.move_by(0);
    }

    fn move_by(&mut self, delta: i32) {
        let selectable: Vec<usize> = (0..self.rows.len()).filter(|i| self.rows[*i].selectable()).collect();
        if selectable.is_empty() {
            return;
        }
        let current = self.state.selected().and_then(|s| selectable.iter().position(|i| *i == s)).unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, selectable.len() as i32 - 1) as usize;
        self.state.select(Some(selectable[next]));
    }

    /// The selectable row under a screen position, if any.
    fn hit(&self, column: u16, row: u16) -> Option<usize> {
        let a = self.inner;
        let inside = column >= a.x && column < a.x + a.width && row >= a.y && row < a.y + a.height;
        let index = self.state.offset() + (row.checked_sub(a.y)? as usize);
        (inside && self.rows.get(index).is_some_and(Row::selectable)).then_some(index)
    }

    /// Flips a row's tick; returns whether it is now ticked.
    fn toggle(&mut self, index: usize) -> bool {
        if self.ticked.remove(&index) {
            false
        } else {
            self.ticked.insert(index);
            true
        }
    }
}

/// What a plan is for: a Teamcraft list, or items typed in the TUI.
#[derive(Clone, PartialEq, Debug)]
pub enum Source {
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

/// The list of markets to pick from, fetched from Universalis at startup.
enum Markets {
    Loading(Receiver<Result<Vec<Region>, String>>),
    Ready(Vec<Region>),
    Failed(String),
}

const FORM_COLUMNS: [(&str, u16); 7] = [
    ("Job", 6),
    ("Level", 8),
    ("Craftsmanship", 15),
    ("Control", 10),
    ("CP", 7),
    ("Specialist", 12),
    ("Manipulation", 13),
];
const GATHER_COLUMNS: [(&str, u16); 3] = [("Job", 6), ("Level", 8), ("Perception", 15)];
/// Form rows: 0-7 crafters, then the three gatherers, then the two crafting preferences.
const FORM_GATHER_ROW: usize = 8;
const FORM_GUARANTEE_ROW: usize = 11;
const FORM_INFEASIBLE_ROW: usize = 12;

fn is_gather_row(row: usize) -> bool {
    (FORM_GATHER_ROW..FORM_GUARANTEE_ROW).contains(&row)
}

enum FormAction {
    Stay,
    Save,
    Cancel,
}

/// The crafter stats editor.
struct CrafterForm {
    /// Numbers as typed, per job: level, craftsmanship, control, CP. Empty = job not set.
    numbers: [[String; 4]; 8],
    specialist: [bool; 8],
    manipulation: [bool; 8],
    /// Per gatherer: level, perception. Empty = not set.
    gather_numbers: [[String; 2]; 3],
    guarantee_hq: bool,
    craft_infeasible: bool,
    /// Selected cell (see the FORM_* rows); columns count from the first cell after the job name.
    row: usize,
    col: usize,
    /// Cell areas from the last draw, for clicks: (area, row, col).
    hits: Vec<(Rect, usize, usize)>,
    error: Option<String>,
}

impl CrafterForm {
    fn new(config: &Config) -> Self {
        let mut form = CrafterForm {
            numbers: Default::default(),
            specialist: [false; 8],
            // Nearly everyone has Manipulation; it's on unless the config says otherwise.
            manipulation: [true; 8],
            gather_numbers: Default::default(),
            guarantee_hq: config.guarantee_hq,
            craft_infeasible: config.craft_infeasible,
            row: 0,
            col: 0,
            hits: Vec::new(),
            error: None,
        };
        for (job, crafter) in config.crafters.iter().enumerate() {
            if let Some(c) = crafter {
                form.numbers[job] =
                    [c.level.to_string(), c.craftsmanship.to_string(), c.control.to_string(), c.cp.to_string()];
                form.specialist[job] = c.specialist;
                form.manipulation[job] = c.manipulation;
            }
        }
        for (job, gatherer) in config.gatherers.iter().enumerate() {
            if let Some(g) = gatherer {
                form.gather_numbers[job] = [g.level.to_string(), g.perception.to_string()];
            }
        }
        form
    }

    /// The edited settings applied on top of `base`, or what's wrong with them.
    fn to_config(&self, base: &Config) -> Result<Config, String> {
        let mut config = base.clone();
        config.guarantee_hq = self.guarantee_hq;
        config.craft_infeasible = self.craft_infeasible;
        for (job, &name) in JOBS.iter().enumerate() {
            let cells = &self.numbers[job];
            if cells.iter().all(|c| c.trim().is_empty()) {
                config.crafters[job] = None;
                continue;
            }
            let field = |i: usize, label: &str| -> Result<u16, String> {
                cells[i]
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| format!("{name}: fill in {label} (or press x on the row to clear it)"))
            };
            let level = field(0, "level")?;
            if !(1..=100).contains(&level) {
                return Err(format!("{name}: level must be between 1 and 100"));
            }
            config.crafters[job] = Some(Crafter {
                level: level as u8,
                craftsmanship: field(1, "craftsmanship")?,
                control: field(2, "control")?,
                cp: field(3, "CP")?,
                specialist: self.specialist[job],
                manipulation: self.manipulation[job],
            });
        }
        for (job, &name) in GATHERERS.iter().enumerate() {
            let cells = &self.gather_numbers[job];
            if cells.iter().all(|c| c.trim().is_empty()) {
                config.gatherers[job] = None;
                continue;
            }
            let field = |i: usize, label: &str| -> Result<u16, String> {
                cells[i]
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| format!("{name}: fill in {label} (or press x on the row to clear it)"))
            };
            let level = field(0, "level")?;
            if !(1..=100).contains(&level) {
                return Err(format!("{name}: level must be between 1 and 100"));
            }
            config.gatherers[job] = Some(Gatherer { level: level as u8, perception: field(1, "perception")? });
        }
        Ok(config)
    }

    /// How many selectable cells a row has.
    fn width(row: usize) -> usize {
        match row {
            r if r < FORM_GATHER_ROW => 6,
            r if is_gather_row(r) => 2,
            _ => 1,
        }
    }

    fn row_count() -> usize {
        FORM_INFEASIBLE_ROW + 1
    }

    fn move_to(&mut self, row: i32, col: i32) {
        self.row = row.clamp(0, Self::row_count() as i32 - 1) as usize;
        self.col = col.clamp(0, Self::width(self.row) as i32 - 1) as usize;
    }

    /// Tab order: across a job's cells, then down to the next job.
    fn step(&mut self, forward: bool) {
        let width = Self::width;
        if forward {
            if self.col + 1 < width(self.row) {
                self.col += 1;
            } else if self.row + 1 < Self::row_count() {
                self.row += 1;
                self.col = 0;
            }
        } else if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = width(self.row) - 1;
        }
    }

    /// Toggles the selected cell if it's a checkbox; returns whether it was one.
    fn toggle(&mut self) -> bool {
        match (self.row, self.col) {
            (FORM_GUARANTEE_ROW, _) => self.guarantee_hq = !self.guarantee_hq,
            (FORM_INFEASIBLE_ROW, _) => self.craft_infeasible = !self.craft_infeasible,
            (job, 4) if job < FORM_GATHER_ROW => self.specialist[job] = !self.specialist[job],
            (job, 5) if job < FORM_GATHER_ROW => self.manipulation[job] = !self.manipulation[job],
            _ => return false,
        }
        true
    }

    fn number_cell(&mut self) -> Option<&mut String> {
        match (self.row, self.col) {
            (r, c) if r < FORM_GATHER_ROW && c < 4 => Some(&mut self.numbers[r][c]),
            (r, c) if is_gather_row(r) => Some(&mut self.gather_numbers[r - FORM_GATHER_ROW][c]),
            _ => None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> FormAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.error = None;
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Char('s') => return FormAction::Save,
            KeyCode::Char('S') if ctrl => return FormAction::Save,
            KeyCode::Up => self.move_to(self.row as i32 - 1, self.col as i32),
            KeyCode::Down => self.move_to(self.row as i32 + 1, self.col as i32),
            KeyCode::Left => self.move_to(self.row as i32, self.col as i32 - 1),
            KeyCode::Right => self.move_to(self.row as i32, self.col as i32 + 1),
            KeyCode::Tab => self.step(true),
            KeyCode::BackTab => self.step(false),
            KeyCode::Char(' ') => {
                self.toggle();
            }
            KeyCode::Enter => {
                if !self.toggle() {
                    self.move_to(self.row as i32 + 1, self.col as i32);
                }
            }
            KeyCode::Char(c @ '0'..='9') => {
                if let Some(cell) = self.number_cell()
                    && cell.len() < 5
                {
                    cell.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(cell) = self.number_cell() {
                    cell.pop();
                }
            }
            KeyCode::Delete => {
                if let Some(cell) = self.number_cell() {
                    cell.clear();
                }
            }
            // Copy the row above: handy when several crafters (or gatherers) share gear.
            KeyCode::Char('d') if self.row > 0 && self.row < FORM_GATHER_ROW => {
                let above = self.row - 1;
                self.numbers[self.row] = self.numbers[above].clone();
                self.specialist[self.row] = self.specialist[above];
                self.manipulation[self.row] = self.manipulation[above];
            }
            KeyCode::Char('d') if is_gather_row(self.row) && self.row > FORM_GATHER_ROW => {
                let g = self.row - FORM_GATHER_ROW;
                self.gather_numbers[g] = self.gather_numbers[g - 1].clone();
            }
            KeyCode::Char('x') if self.row < FORM_GATHER_ROW => self.numbers[self.row] = Default::default(),
            KeyCode::Char('x') if is_gather_row(self.row) => {
                self.gather_numbers[self.row - FORM_GATHER_ROW] = Default::default()
            }
            _ => {}
        }
        FormAction::Stay
    }

    fn click(&mut self, column: u16, row: u16) {
        let Some(&(_, r, c)) = self
            .hits
            .iter()
            .find(|(a, _, _)| column >= a.x && column < a.x + a.width && row >= a.y && row < a.y + a.height)
        else {
            return;
        };
        self.row = r;
        self.col = c;
        // Checkboxes toggle on click; number cells just get selected.
        self.toggle();
    }
}

/// The market chooser popup.
#[derive(Default)]
struct Picker {
    filter: String,
    state: ListState,
    /// Where the list was last drawn, for mouse hit-testing.
    inner: Rect,
}

/// One choosable market: a region, data center or world.
#[derive(Debug, PartialEq)]
struct MarketEntry {
    label: String,
    value: String,
    /// 0 = region, 1 = data center, 2 = world.
    depth: u8,
}

/// Regions, then their data centers, then worlds (all alphabetical). When filtering, matches
/// are listed flat with where they belong.
fn market_entries(regions: &[Region], filter: &str) -> Vec<MarketEntry> {
    let filter = filter.trim().to_lowercase();
    let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
    let mut out = Vec::new();
    let mut push = |label: String, value: &str, depth: u8| {
        out.push(MarketEntry { label, value: value.to_string(), depth: if filter.is_empty() { depth } else { 0 } })
    };
    for region in regions {
        if matches(&region.name) {
            let label = if filter.is_empty() { region.name.clone() } else { format!("{} (region)", region.name) };
            push(label, &region.name, 0);
        }
        for dc in &region.data_centers {
            if matches(&dc.name) {
                let label = if filter.is_empty() {
                    dc.name.clone()
                } else {
                    format!("{} (data center, {})", dc.name, region.name)
                };
                push(label, &dc.name, 1);
            }
            for world in &dc.worlds {
                if matches(world) {
                    let label = if filter.is_empty() {
                        world.clone()
                    } else {
                        format!("{world} ({}, {})", dc.name, region.name)
                    };
                    push(label, world, 2);
                }
            }
        }
    }
    out
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
    picker: Option<Picker>,
    form: Option<CrafterForm>,
    /// Whether to save settings (like the market) to config.toml; off in tests.
    persist: bool,
    markets: Markets,
    /// A plan requested before a market was chosen; runs once one is.
    pending: Option<Source>,

    tab: usize,
    market: Pane,
    crafts: Pane,
    scroll: u16,

    message: Option<(String, Instant)>,
    clipboard: Option<arboard::Clipboard>,
    watch_clipboard: bool,
    last_clip: Option<String>,
    last_clip_check: Instant,

    // Screen geometry from the last draw, for mouse hit-testing.
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
            picker: None,
            form: None,
            persist: false,
            markets: Markets::Failed("not loaded".into()),
            pending: None,
            tab: 0,
            market: Pane::default(),
            crafts: Pane::default(),
            scroll: 0,
            message: None,
            clipboard: arboard::Clipboard::new().ok(),
            watch_clipboard,
            last_clip: None,
            last_clip_check: Instant::now(),
            tab_hits: Vec::new(),
            quit: false,
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                // A paste arrives as a rapid stream of key presses (on Windows) that may not all
                // be queued yet, so keep reading until input pauses. Typing and key repeat are
                // much slower than this gap, so they still arrive one key at a time.
                let started = Instant::now();
                let mut batch = vec![event::read()?];
                while started.elapsed() < Duration::from_secs(2) && event::poll(PASTE_GAP)? {
                    batch.push(event::read()?);
                }
                self.handle_batch(batch);
            }
            self.poll_planner();
            self.poll_markets();
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
        if self.form.is_some() {
            for event in batch {
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            self.quit = true;
                            return;
                        }
                        let action = self.form.as_mut().map_or(FormAction::Stay, |f| f.key(key));
                        match action {
                            FormAction::Stay => {}
                            FormAction::Cancel => self.form = None,
                            FormAction::Save => self.save_form(),
                        }
                    }
                    Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, .. }) => {
                        if let Some(form) = &mut self.form {
                            form.click(column, row);
                        }
                    }
                    _ => {}
                }
                if self.form.is_none() {
                    return;
                }
            }
            return;
        }
        if self.picker.is_some() {
            for event in batch {
                match event {
                    Event::Paste(text) => self.picker_filter(|f| f.push_str(text.trim())),
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.picker_key(key),
                    Event::Mouse(mouse) => self.picker_mouse(mouse),
                    _ => {}
                }
            }
            return;
        }
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
        let text = &strip_paste_markers(text);
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
                self.set_market(text);
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

    /// Switches market, saves it to the config, and re-plans what was waiting on it (or the
    /// current plan, at the new market's prices).
    fn set_market(&mut self, market: String) {
        let changed = !self.opts.market.eq_ignore_ascii_case(&market);
        self.opts.market = market;
        match self.pending.take() {
            Some(source) => self.start_plan(source),
            None => match self.last_source.clone() {
                Some(source) if changed => self.start_plan(source),
                _ => self.flash(&format!("Market set to {}", self.opts.market)),
            },
        }
        if self.persist
            && let Err(e) = config::save_market(&self.opts.market)
        {
            self.flash(&format!("Couldn't save the market to config.toml: {e:#}"));
        }
    }

    /// Applies the crafter form: saves it to config.toml and re-plans with the new stats.
    fn save_form(&mut self) {
        let Some(form) = &mut self.form else { return };
        let config = match form.to_config(&self.opts.config) {
            Ok(config) => config,
            Err(e) => {
                form.error = Some(e);
                return;
            }
        };
        self.form = None;
        self.opts.config = config;
        let saved = if self.persist { config::save_crafting(&self.opts.config) } else { Ok(()) };
        match self.last_source.clone() {
            // Stats change which crafts are possible and every macro, so plan again.
            Some(source) => self.start_plan(source),
            None => self.flash("Crafter stats saved"),
        }
        if let Err(e) = saved {
            self.flash(&format!("Couldn't save to config.toml: {e:#}"));
        }
    }

    // ----- market picker -----

    /// Loads the list of regions, data centers and worlds in the background.
    fn load_markets(&mut self) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = Http::new(false, |_| {}).and_then(|http| universalis::markets(&http));
            let _ = tx.send(result.map_err(|e| format!("{e:#}")));
        });
        self.markets = Markets::Loading(rx);
    }

    fn poll_markets(&mut self) {
        let Markets::Loading(rx) = &self.markets else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("stopped unexpectedly".into()),
        };
        match result {
            Ok(regions) => {
                self.markets = Markets::Ready(regions);
                if let Some(picker) = &mut self.picker {
                    picker.state.select(Some(0));
                    self.select_current_market();
                }
            }
            Err(e) => {
                self.markets = Markets::Failed(e);
                // Without the list, fall back to typing the market.
                if self.picker.take().is_some() {
                    self.open_market_picker();
                }
            }
        }
    }

    fn open_market_picker(&mut self) {
        if let Markets::Failed(reason) = &self.markets {
            let msg = format!("Couldn't load the market list ({reason}), so type it instead");
            self.flash(&msg);
            self.open_prompt(PromptKind::Market);
            return;
        }
        self.picker = Some(Picker::default());
        self.select_current_market();
    }

    fn picker_entries(&self) -> Vec<MarketEntry> {
        match (&self.markets, &self.picker) {
            (Markets::Ready(regions), Some(picker)) => market_entries(regions, &picker.filter),
            _ => Vec::new(),
        }
    }

    fn select_current_market(&mut self) {
        let entries = self.picker_entries();
        let index = entries.iter().position(|e| e.value.eq_ignore_ascii_case(&self.opts.market)).unwrap_or(0);
        if let Some(picker) = &mut self.picker {
            picker.state.select(Some(index));
        }
    }

    fn picker_move(&mut self, delta: i32) {
        let len = self.picker_entries().len();
        let Some(picker) = &mut self.picker else { return };
        if len == 0 {
            return;
        }
        let current = picker.state.selected().unwrap_or(0) as i32;
        picker.state.select(Some((current + delta).clamp(0, len as i32 - 1) as usize));
    }

    fn picker_choose(&mut self, index: usize) {
        let Some(entry) = self.picker_entries().into_iter().nth(index) else { return };
        self.picker = None;
        self.set_market(entry.value);
    }

    fn picker_filter(&mut self, edit: impl FnOnce(&mut String)) {
        if let Some(picker) = &mut self.picker {
            edit(&mut picker.filter);
            picker.state.select(Some(0));
        }
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Esc => {
                self.picker = None;
                self.pending = None;
            }
            KeyCode::Enter => {
                if let Some(i) = self.picker.as_ref().and_then(|p| p.state.selected()) {
                    self.picker_choose(i);
                }
            }
            KeyCode::Down => self.picker_move(1),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::PageDown => self.picker_move(10),
            KeyCode::PageUp => self.picker_move(-10),
            KeyCode::Home => self.picker_move(i32::MIN / 2),
            KeyCode::End => self.picker_move(i32::MAX / 2),
            KeyCode::Backspace => self.picker_filter(|f| {
                f.pop();
            }),
            KeyCode::Char('u') if ctrl => self.picker_filter(String::clear),
            KeyCode::Char(c) if !ctrl => self.picker_filter(|f| f.push(c)),
            _ => {}
        }
    }

    fn picker_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(picker) = &self.picker else { return };
                let a = picker.inner;
                if mouse.column >= a.x && mouse.column < a.x + a.width && mouse.row >= a.y && mouse.row < a.y + a.height
                {
                    let index = picker.state.offset() + (mouse.row - a.y) as usize;
                    self.picker_choose(index);
                }
            }
            MouseEventKind::ScrollDown => self.picker_move(3),
            MouseEventKind::ScrollUp => self.picker_move(-3),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            // Not Esc: a stray Esc (e.g. from a terminal's paste markers) shouldn't close the app.
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('v') if ctrl => self.import_from_clipboard(),
            KeyCode::Char('p') => self.import_from_clipboard(),
            KeyCode::Char('m') => self.open_market_picker(),
            KeyCode::Char('c') if !ctrl => self.form = Some(CrafterForm::new(&self.opts.config)),
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
                if let Some(i) = self.pane().and_then(|p| p.state.selected()) {
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
                if let Some(pane) = self.pane_mut()
                    && let Some(index) = pane.hit(mouse.column, mouse.row)
                {
                    pane.state.select(Some(index));
                    self.activate(index);
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

    /// The clickable list on the current tab (Market and Crafts have one).
    fn pane(&self) -> Option<&Pane> {
        match self.tab {
            0 => Some(&self.market),
            2 => Some(&self.crafts),
            _ => None,
        }
    }

    fn pane_mut(&mut self) -> Option<&mut Pane> {
        match self.tab {
            0 => Some(&mut self.market),
            2 => Some(&mut self.crafts),
            _ => None,
        }
    }

    fn move_by(&mut self, delta: i32) {
        match self.pane_mut() {
            Some(pane) => pane.move_by(delta),
            None => self.scroll = (self.scroll as i32 + delta).max(0) as u16,
        }
    }

    /// Copies the selected row (an item name, or a crafting macro) and toggles its tick.
    fn activate(&mut self, index: usize) {
        let Some(view) = &self.view else { return };
        let (text, what, done) = match self.pane().and_then(|p| p.rows.get(index)) {
            Some(Row::Line { group, line }) => {
                let name = view.market[*group].lines[*line].item.clone();
                (name.clone(), format!("\"{name}\""), "bought")
            }
            Some(Row::Shortfall(i)) => {
                let name = view.shortfalls[*i].item.clone();
                (name.clone(), format!("\"{name}\""), "bought")
            }
            Some(Row::Macro { step, block }) => {
                let b = &view.crafts[*step].macros[*block];
                (b.text.clone(), b.label.clone(), "done")
            }
            _ => return,
        };
        let copied = self.copy(&text);
        let ticked = self.pane_mut().is_some_and(|p| p.toggle(index));
        let mark = if ticked { format!("marked {done}") } else { "unmarked".to_string() };
        let copy_note = if copied { format!("Copied {what}") } else { format!("Couldn't copy {what}") };
        self.flash(&format!("{copy_note} · {mark}"));
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
            self.open_market_picker();
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
        self.market.reset(rows);

        let mut rows = Vec::new();
        for (i, step) in view.crafts.iter().enumerate() {
            rows.push(Row::Craft(i));
            rows.extend(step.details.iter().map(|d| Row::Text(format!("    {d}"))));
            rows.extend((0..step.macros.len()).map(|block| Row::Macro { step: i, block }));
            rows.push(Row::Blank);
        }
        if view.crafts.is_empty() {
            rows.push(Row::Text("Nothing to craft: the plan buys the finished items.".into()));
        }
        self.crafts.reset(rows);

        self.view = Some(view);
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
                2 => self.draw_crafts(f, body),
                _ => self.draw_text(f, body, summary_lines(self.view.as_ref().unwrap())),
            },
            None => self.draw_placeholder(f, body),
        }
        self.draw_footer(f, footer);
        self.draw_prompt(f);
        self.draw_picker(f);
        self.draw_form(f);
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
            .market
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                Row::Heading(g) => {
                    let group = &view.market[*g];
                    let left: u64 = self
                        .market
                        .rows
                        .iter()
                        .enumerate()
                        .filter(|(j, r)| {
                            matches!(r, Row::Line { group, .. } if group == g) && !self.market.ticked.contains(j)
                        })
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
                    checkbox_item(&text, self.market.ticked.contains(&i))
                }
                Row::Shortfall(s) => {
                    let short = &view.shortfalls[*s];
                    let text = match &short.listings {
                        Some(l) => format!("{} — {l}", short.summary),
                        None => short.summary.clone(),
                    };
                    checkbox_item(&text, self.market.ticked.contains(&i))
                }
                Row::Text(t) => ListItem::new(Line::from(t.clone().dim())),
                Row::Blank | Row::Craft(_) | Row::Macro { .. } => ListItem::new(""),
            })
            .collect();
        let block = Block::default().borders(Borders::ALL).title(" Market board · click or Enter to copy & tick ");
        self.market.inner = block.inner(area);
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.market.state);
    }

    fn draw_crafts(&mut self, f: &mut Frame, area: Rect) {
        let view = self.view.as_ref().unwrap();
        let items: Vec<ListItem> = self
            .crafts
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                Row::Craft(s) => ListItem::new(Line::from(view.crafts[*s].line.clone().cyan().bold())),
                Row::Macro { step, block } => {
                    checkbox_item(&view.crafts[*step].macros[*block].label, self.crafts.ticked.contains(&i))
                }
                Row::Text(t) => ListItem::new(Line::from(t.clone().dim())),
                _ => ListItem::new(""),
            })
            .collect();

        // With room to spare, show the selected macro's text beside the list.
        let selected_macro = self.crafts.state.selected().and_then(|i| match self.crafts.rows.get(i) {
            Some(Row::Macro { step, block }) => Some(&view.crafts[*step].macros[*block]),
            _ => None,
        });
        let (list_area, preview_area) = if area.width >= 110 && selected_macro.is_some() {
            let [l, r] = Layout::horizontal([Constraint::Min(60), Constraint::Length(48)]).areas(area);
            (l, Some(r))
        } else {
            (area, None)
        };
        let block =
            Block::default().borders(Borders::ALL).title(" Crafts, in order · click or Enter to copy a macro & tick ");
        self.crafts.inner = block.inner(list_area);
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        if let (Some(area), Some(m)) = (preview_area, selected_macro) {
            let preview = Paragraph::new(m.text.lines().map(|l| Line::from(l.to_string())).collect::<Vec<_>>())
                .block(Block::default().borders(Borders::ALL).title(" Macro "));
            f.render_widget(preview, area);
        }
        f.render_stateful_widget(list, list_area, &mut self.crafts.state);
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
            if !self.opts.config.any_crafters() {
                lines.push(Line::from(""));
                lines.push(Line::from("Press c to enter your crafter stats and get crafting macros.".dim()));
            }
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
        let keys = if self.form.is_some() {
            "type numbers · arrows/Tab move · Space toggle · d copy row above · x clear · s save · Esc cancel"
        } else if self.picker.is_some() {
            "click/Enter choose · ↑↓ move · type to filter · Backspace edit filter · Esc cancel"
        } else if self.prompt.is_some() {
            "Enter plan · Esc cancel · Ctrl+V paste · Ctrl+U clear"
        } else {
            "p/Ctrl+V paste link · i items · m market · c crafters · click/Enter copy+tick · Tab views · r reload · q quit"
        };
        f.render_widget(Paragraph::new(Line::from(keys.dim())), help);
    }

    fn draw_form(&mut self, f: &mut Frame) {
        let Some(form) = &mut self.form else { return };
        let area = f.area();
        let table_width: u16 = FORM_COLUMNS.iter().map(|(_, w)| w).sum();
        let width = (table_width + 4).min(area.width);
        let height = 25.min(area.height);
        let popup =
            Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Crafter & gatherer stats (base stats, without food or potions) ");
        let inner = block.inner(popup);
        f.render_widget(Clear, popup);
        f.render_widget(block, popup);
        form.hits.clear();

        let selected = Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD);
        let mut cell = |f: &mut Frame, text: String, x: u16, y: u16, w: u16, pos: Option<(usize, usize)>, dim: bool| {
            if y >= inner.y + inner.height || x >= inner.x + inner.width {
                return;
            }
            let rect = Rect { x, y, width: w.min(inner.x + inner.width - x), height: 1 };
            let mut style = if dim { Style::default().fg(Color::DarkGray) } else { Style::default() };
            if let Some((r, c)) = pos {
                if (form.row, form.col) == (r, c) {
                    style = selected;
                }
                form.hits.push((rect, r, c));
            }
            f.render_widget(Paragraph::new(Span::styled(text, style)), rect);
        };

        let check = |on: bool| if on { "[x]" } else { "[ ]" };
        // Header.
        let mut x = inner.x + 1;
        for (title, w) in FORM_COLUMNS {
            cell(f, format!("{title:<width$}", width = w as usize - 1), x, inner.y, w, None, true);
            x += w;
        }
        // One row per job.
        for (job, name) in JOBS.iter().enumerate() {
            let y = inner.y + 1 + job as u16;
            let unset = form.numbers[job].iter().all(|n| n.is_empty());
            let mut x = inner.x + 1;
            cell(f, name.to_string(), x, y, FORM_COLUMNS[0].1, None, false);
            x += FORM_COLUMNS[0].1;
            for (i, (_, w)) in FORM_COLUMNS[1..5].iter().enumerate() {
                let n = &form.numbers[job][i];
                let text = if n.is_empty() { "—".to_string() } else { n.clone() };
                cell(f, format!(" {text:<width$}", width = *w as usize - 2), x, y, w - 1, Some((job, i)), unset);
                x += w;
            }
            for (i, on) in [(4, form.specialist[job]), (5, form.manipulation[job])] {
                let w = FORM_COLUMNS[i + 1].1;
                cell(f, format!(" {}", check(on)), x, y, 5, Some((job, i)), unset);
                x += w;
            }
        }
        // Gatherers, after a blank line.
        let mut x = inner.x + 1;
        for (title, w) in GATHER_COLUMNS {
            cell(f, format!("{title:<width$}", width = w as usize - 1), x, inner.y + 10, w, None, true);
            x += w;
        }
        for (g, name) in GATHERERS.iter().enumerate() {
            let y = inner.y + 11 + g as u16;
            let row = FORM_GATHER_ROW + g;
            let unset = form.gather_numbers[g].iter().all(|n| n.is_empty());
            let mut x = inner.x + 1;
            cell(f, name.to_string(), x, y, GATHER_COLUMNS[0].1, None, false);
            x += GATHER_COLUMNS[0].1;
            for (i, (_, w)) in GATHER_COLUMNS[1..].iter().enumerate() {
                let n = &form.gather_numbers[g][i];
                let text = if n.is_empty() { "—".to_string() } else { n.clone() };
                cell(f, format!(" {text:<width$}", width = *w as usize - 2), x, y, w - 1, Some((row, i)), unset);
                x += w;
            }
        }
        // Preferences.
        let y = inner.y + 15;
        let guarantee =
            format!(" {} Guarantee HQ (holds even if every condition rolls Poor)", check(form.guarantee_hq));
        cell(f, guarantee, inner.x + 1, y, inner.width - 2, Some((FORM_GUARANTEE_ROW, 0)), false);
        let infeasible =
            format!(" {} Plan crafts my stats can't finish (otherwise buy them)", check(form.craft_infeasible));
        cell(f, infeasible, inner.x + 1, y + 1, inner.width - 2, Some((FORM_INFEASIBLE_ROW, 0)), false);

        let mut help = vec![
            Line::from(""),
            Line::from(" Empty (—) crafters get no macros; empty gatherers are assumed to gather anything.".dim()),
            Line::from(" Specialists get Heart and Soul and Quick Innovation.".dim()),
        ];
        if let Some(err) = &form.error {
            help.push(Line::from(""));
            help.push(Line::from(format!(" {err}").red().bold()));
        }
        let help_area = Rect { x: inner.x, y: y + 2, width: inner.width, height: inner.height.saturating_sub(17) };
        f.render_widget(Paragraph::new(help).wrap(Wrap { trim: false }), help_area);
    }

    fn draw_picker(&mut self, f: &mut Frame) {
        let entries = self.picker_entries();
        let loading = matches!(self.markets, Markets::Loading(_));
        let current = self.opts.market.clone();
        let Some(picker) = &mut self.picker else { return };
        let area = f.area();
        let width = area.width.saturating_sub(4).min(64);
        let height = area.height.saturating_sub(2).min(28);
        let popup =
            Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Choose a market ");
        let inner = block.inner(popup);
        f.render_widget(Clear, popup);
        f.render_widget(block, popup);

        let [filter_area, list_area, hint_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)]).areas(inner);
        let filter = Line::from(vec![
            Span::styled(" Filter: ", Style::default().fg(Color::Gray)),
            Span::raw(picker.filter.clone()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]);
        f.render_widget(Paragraph::new(filter), filter_area);
        f.render_widget(
            Paragraph::new(" A region or data center compares all its worlds (world visits are free).".dim())
                .wrap(Wrap { trim: true }),
            hint_area,
        );

        picker.inner = list_area;
        if entries.is_empty() {
            let text = if loading { " Loading markets..." } else { " No market matches that filter." };
            f.render_widget(Paragraph::new(text.dim()), list_area);
            return;
        }
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let style = match e.depth {
                    0 => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    1 => Style::default().fg(Color::Cyan),
                    _ => Style::default(),
                };
                let mut spans = vec![Span::styled(format!("{}{}", "  ".repeat(e.depth as usize), e.label), style)];
                if e.value.eq_ignore_ascii_case(&current) {
                    spans.push(Span::styled("  ● current", Style::default().fg(Color::Green)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, list_area, &mut picker.state);
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
        self.market
            .rows
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.market.ticked.contains(i))
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
    if !view.blocked.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Why these are bought instead of crafted or gathered:".cyan().bold()));
        lines.extend(view.blocked.iter().map(|l| Line::from(format!("  • {l}"))));
    }
    for note in &view.notes {
        lines.push(Line::from(""));
        lines.push(Line::from(note.clone().dim()));
    }
    lines
}

/// What to plan from command-line input: an explicit Teamcraft list, a lone Teamcraft link
/// or list id, or otherwise item entries ("3x Iron Ingot", ...).
pub fn source_from_args(entries: Vec<String>, teamcraft: Option<String>) -> Option<Source> {
    if let Some(list) = teamcraft {
        return Some(Source::Teamcraft(list));
    }
    if let [only] = entries.as_slice()
        && let Some(link) = teamcraft_link(only)
    {
        return Some(Source::Teamcraft(link));
    }
    let items: Vec<String> = entries.into_iter().filter(|e| crate::input::parse_entry(e).is_some()).collect();
    (!items.is_empty()).then_some(Source::Items(items))
}

/// Removes bracketed-paste markers (ESC[200~ / ESC[201~) a terminal may wrap a paste in, in
/// case they arrive as plain keys.
fn strip_paste_markers(text: &str) -> String {
    text.replace('\u{1b}', "").replace("[200~", "").replace("[201~", "")
}

/// Picks a Teamcraft list link (or bare list id) out of pasted text.
fn teamcraft_link(text: &str) -> Option<String> {
    let text = &strip_paste_markers(text);
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::view::{CraftStep, MacroBlock, MarketLine, WorldGroup};
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
            crafts: vec![CraftStep {
                line: "[ALC] 2× Grade 2 Gemsap of Vitality (1 craft) · HQ".into(),
                details: vec!["Use HQ 1× Something in each craft".into()],
                macros: vec![MacroBlock {
                    label: "HQ macro for 1 craft (2 steps, 6s)".into(),
                    text: "/ac \"Reflect\" <wait.3>\n/echo done <se.1>".into(),
                }],
            }],
            shortfalls: vec![],
            blocked: vec![],
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
            config: crate::config::Config::default(),
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
        app.handle_batch(vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })]);
    }

    #[test]
    fn clicking_a_market_line_ticks_it_and_reduces_what_is_left() {
        let mut app = test_app();
        let mut term = Terminal::new(TestBackend::new(110, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let before = app.remaining_cost();

        let row = screen_row(&term, "3× Hippogryph Skin");
        click(&mut app, 12, row);
        assert_eq!(app.market.ticked.len(), 1);
        assert!(app.remaining_cost() < before);
        assert!(app.message.as_ref().unwrap().0.contains("Hippogryph Skin"));

        // Clicking a world heading does nothing; clicking the line again unticks it.
        term.draw(|f| app.draw(f)).unwrap();
        click(&mut app, 12, screen_row(&term, "Diabolos"));
        assert_eq!(app.market.ticked.len(), 1);
        click(&mut app, 12, row);
        assert!(app.market.ticked.is_empty());
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
    fn clicking_a_macro_copies_it_and_ticks_it() {
        let mut app = test_app();
        app.set_tab(2);
        let mut term = Terminal::new(TestBackend::new(130, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        // The selected macro's text is previewed beside the list on wide screens.
        screen_row(&term, "/ac \"Reflect\" <wait.3>");

        click(&mut app, 12, screen_row(&term, "HQ macro for 1 craft"));
        assert_eq!(app.crafts.ticked.len(), 1);
        assert!(app.market.ticked.is_empty());
        let message = &app.message.as_ref().unwrap().0;
        assert!(message.contains("Copied HQ macro") && message.contains("marked done"), "{message}");

        // Craft headings and details aren't clickable.
        click(&mut app, 12, screen_row(&term, "Grade 2 Gemsap"));
        assert_eq!(app.crafts.ticked.len(), 1);
    }

    fn sample_regions() -> Vec<Region> {
        use crate::universalis::DataCenter;
        let dc = |name: &str, worlds: &[&str]| DataCenter {
            name: name.into(),
            worlds: worlds.iter().map(|w| w.to_string()).collect(),
        };
        vec![
            Region { name: "Europe".into(), data_centers: vec![dc("Chaos", &["Cerberus", "Omega"])] },
            Region {
                name: "North-America".into(),
                data_centers: vec![dc("Aether", &["Gilgamesh", "Siren"]), dc("Crystal", &["Balmung", "Coeurl"])],
            },
        ]
    }

    #[test]
    fn market_entries_are_grouped_and_filterable() {
        let regions = sample_regions();
        let tree: Vec<(String, u8)> = market_entries(&regions, "").into_iter().map(|e| (e.label, e.depth)).collect();
        let expected = [
            ("Europe", 0),
            ("Chaos", 1),
            ("Cerberus", 2),
            ("Omega", 2),
            ("North-America", 0),
            ("Aether", 1),
            ("Gilgamesh", 2),
            ("Siren", 2),
            ("Crystal", 1),
            ("Balmung", 2),
            ("Coeurl", 2),
        ];
        assert_eq!(tree, expected.map(|(l, d)| (l.to_string(), d)));

        let found = market_entries(&regions, "coe");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].value, "Coeurl");
        assert_eq!(found[0].label, "Coeurl (Crystal, North-America)");
    }

    #[test]
    fn picking_a_market_by_filter_and_enter() {
        let mut app = test_app();
        app.opts.market.clear();
        app.markets = Markets::Ready(sample_regions());

        app.handle_batch(vec![key(KeyCode::Char('m'))]);
        assert!(app.picker.is_some() && app.prompt.is_none());
        // Typing filters instead of triggering shortcuts ('r' would otherwise reload).
        type_text(&mut app, "crystal");
        assert!(!app.quit);
        let entries = app.picker_entries();
        assert_eq!(entries.first().map(|e| e.value.as_str()), Some("Crystal"));

        // Nothing was waiting on a market, so choosing just sets it (no plan, no network).
        app.handle_batch(vec![key(KeyCode::Enter)]);
        assert!(app.picker.is_none());
        assert_eq!(app.opts.market, "Crystal");
        assert!(app.loading.is_none());
    }

    #[test]
    fn clicking_a_market_in_the_picker_chooses_it() {
        let mut app = test_app();
        // No plan behind the popup, so "Coeurl" only appears in the picker.
        app.view = None;
        app.markets = Markets::Ready(sample_regions());
        app.open_market_picker();
        // The current market starts selected.
        let current = app.picker.as_ref().unwrap().state.selected().unwrap();
        assert_eq!(app.picker_entries()[current].value, "Crystal");

        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        click(&mut app, 50, screen_row(&term, "Coeurl"));
        assert!(app.picker.is_none());
        assert_eq!(app.opts.market, "Coeurl");
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_batch(vec![key(code)]);
    }

    #[test]
    fn crafter_form_edits_and_saves_stats() {
        let mut app = test_app();
        press(&mut app, KeyCode::Char('c'));
        assert!(app.form.is_some());

        // CRP: level, craftsmanship, control, CP, then toggle Specialist.
        type_text(&mut app, "100");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "5400");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "5100");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "700");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char(' '));
        // BSM: copy the row above, then fix its level.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('d'));
        for _ in 0..4 {
            press(&mut app, KeyCode::Left);
        }
        for _ in 0..3 {
            press(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "90");
        // MIN (the first gatherer row): level and perception.
        for _ in 0..7 {
            press(&mut app, KeyCode::Down);
        }
        type_text(&mut app, "100");
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "5100");
        // Turn off the HQ guarantee, below the gatherers.
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);

        press(&mut app, KeyCode::Char('s'));
        assert!(app.form.is_none(), "{:?}", app.form.as_ref().and_then(|f| f.error.clone()));
        let config = &app.opts.config;
        let crp = config.crafters[0].unwrap();
        assert_eq!((crp.level, crp.craftsmanship, crp.control, crp.cp, crp.specialist), (100, 5400, 5100, 700, true));
        let bsm = config.crafters[1].unwrap();
        assert_eq!((bsm.level, bsm.craftsmanship, bsm.specialist), (90, 5400, true));
        assert!(config.crafters[2..].iter().all(Option::is_none));
        assert!(!config.guarantee_hq);
        assert_eq!(config.gatherers[0], Some(Gatherer { level: 100, perception: 5100 }));
        assert!(config.gatherers[1].is_none());
        // No plan was loaded, so nothing re-plans (and nothing touches the network).
        assert!(app.loading.is_none());
    }

    #[test]
    fn crafter_form_rejects_half_filled_rows_and_esc_discards() {
        let mut app = test_app();
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "100");
        press(&mut app, KeyCode::Char('s'));
        let form = app.form.as_ref().expect("form stays open on error");
        assert!(form.error.as_deref().unwrap().contains("CRP: fill in craftsmanship"));

        press(&mut app, KeyCode::Esc);
        assert!(app.form.is_none());
        assert!(!app.opts.config.any_crafters());
    }

    #[test]
    fn crafter_form_checkboxes_toggle_on_click() {
        let mut app = test_app();
        app.view = None;
        press(&mut app, KeyCode::Char('c'));
        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();

        let form = app.form.as_ref().unwrap();
        let (area, _, _) = *form.hits.iter().find(|(_, r, c)| (*r, *c) == (2, 4)).unwrap();
        assert!(!form.specialist[2]);
        click(&mut app, area.x + 1, area.y);
        let form = app.form.as_ref().unwrap();
        assert!(form.specialist[2]);
        assert_eq!((form.row, form.col), (2, 4));
        screen_row(&term, "Guarantee HQ");
    }

    #[test]
    fn windows_terminal_style_paste_imports_instead_of_quitting() {
        let mut app = test_app();
        // No market, so the plan is queued instead of starting network requests.
        app.opts.market.clear();
        let url = "https://ffxivteamcraft.com/list/abcDEF123ghiJKL456";
        // What a terminal sends for a bracketed paste when it arrives as plain key presses.
        let mut batch = vec![key(KeyCode::Esc)];
        batch.extend(format!("[200~{url}").chars().map(|c| key(KeyCode::Char(c))));
        batch.push(key(KeyCode::Esc));
        batch.extend("[201~".chars().map(|c| key(KeyCode::Char(c))));
        app.handle_batch(batch);

        assert!(!app.quit);
        assert_eq!(app.pending, Some(Source::Teamcraft(url.into())));
    }

    #[test]
    fn esc_does_not_quit_but_q_does() {
        let mut app = test_app();
        press(&mut app, KeyCode::Esc);
        assert!(!app.quit);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn command_line_input_becomes_a_plan_source() {
        let link = "https://ffxivteamcraft.com/list/abcDEF123ghiJKL456".to_string();
        assert_eq!(source_from_args(vec![link.clone()], None), Some(Source::Teamcraft(link.clone())));
        assert_eq!(source_from_args(vec![], Some("someListId".into())), Some(Source::Teamcraft("someListId".into())));
        assert_eq!(
            source_from_args(vec!["3x Iron Ingot".into(), "# comment".into(), "Cobalt Ingot:5".into()], None),
            Some(Source::Items(vec!["3x Iron Ingot".into(), "Cobalt Ingot:5".into()]))
        );
        assert_eq!(source_from_args(vec![], None), None);
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
