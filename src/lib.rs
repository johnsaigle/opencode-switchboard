use std::{env, ffi::OsString, io, process::Command, time::Duration};

use anyhow::{anyhow, Context};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    DefaultTerminal, Frame,
};

const TMUX_FORMAT: &str =
    "#{session_name}:#{window_index}.#{pane_index}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_title}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Live,
}

impl PaneState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub target: String,
    pub pid: u32,
    pub current_path: String,
    pub current_command: String,
    pub pane_title: String,
    pub state: PaneState,
}

impl Pane {
    pub fn display_title(&self) -> &str {
        self.pane_title
            .strip_prefix("OC | ")
            .unwrap_or(&self.pane_title)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filtering,
}

#[derive(Debug)]
struct App {
    panes: Vec<Pane>,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
    mode: InputMode,
    message: String,
}

impl App {
    fn with_filter(panes: Vec<Pane>, filter: String) -> Self {
        let mut app = Self {
            panes,
            filtered: Vec::new(),
            selected: 0,
            filter,
            mode: InputMode::Normal,
            message: String::new(),
        };
        app.apply_filter();
        app
    }

    fn selected_pane(&self) -> Option<&Pane> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.panes.get(*index))
    }

    fn apply_filter(&mut self) {
        self.filtered = filter_panes(&self.panes, &self.filter);
        if self.filtered.is_empty() || self.selected >= self.filtered.len() {
            self.selected = 0;
        }
    }

    fn refresh(&mut self) {
        match discover_tmux_panes() {
            Ok(panes) => {
                self.panes = panes;
                self.apply_filter();
                self.message = format!("refreshed {} OpenCode pane(s)", self.panes.len());
            }
            Err(err) => {
                self.message = format!("refresh failed: {err:#}");
            }
        }
    }

    fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

pub fn run() -> anyhow::Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    let panes = discover_tmux_panes().unwrap_or_else(|err| {
        eprintln!("failed to discover tmux panes: {err:#}");
        Vec::new()
    });
    let app = App::with_filter(panes, options.search.unwrap_or_default());
    run_tui(app)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    search: Option<String>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Self> {
        let mut options = Self::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            let arg = arg
                .into_string()
                .map_err(|_| anyhow!("arguments must be valid UTF-8"))?;

            if let Some(search) = arg.strip_prefix("--search=") {
                options.search = Some(search.to_owned());
            } else if let Some(search) = arg.strip_prefix("-s=") {
                options.search = Some(search.to_owned());
            } else if arg == "--search" || arg == "-s" {
                let search = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires a search phrase"))?
                    .into_string()
                    .map_err(|_| anyhow!("search phrase must be valid UTF-8"))?;
                options.search = Some(search);
            } else if arg == "--help" || arg == "-h" {
                return Err(anyhow!(usage()));
            } else {
                return Err(anyhow!("unknown argument: {arg}\n{}", usage()));
            }
        }

        Ok(options)
    }
}

fn usage() -> &'static str {
    "usage: opencode-switchboard [-s|--search <phrase>]"
}

pub fn discover_tmux_panes() -> anyhow::Result<Vec<Pane>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", TMUX_FORMAT])
        .output()
        .context("failed to execute tmux list-panes")?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux list-panes exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8(output.stdout).context("tmux output was not UTF-8")?;
    let mut panes = stdout
        .lines()
        .filter_map(parse_tmux_pane_line)
        .filter(is_opencode_pane)
        .collect::<Vec<_>>();
    sort_panes(&mut panes);
    Ok(panes)
}

pub fn parse_tmux_pane_line(line: &str) -> Option<Pane> {
    let mut parts = line.splitn(5, '\t');
    let target = parts.next()?.to_owned();
    let pid = parts.next()?.parse().ok()?;
    let current_path = parts.next()?.to_owned();
    let current_command = parts.next()?.to_owned();
    let pane_title = parts.next()?.to_owned();

    Some(Pane {
        target,
        pid,
        current_path,
        current_command,
        pane_title,
        state: PaneState::Live,
    })
}

pub fn filter_panes(panes: &[Pane], filter: &str) -> Vec<usize> {
    let filter = filter.trim().to_lowercase();
    panes
        .iter()
        .enumerate()
        .filter_map(|(index, pane)| {
            if filter.is_empty()
                || pane.display_title().to_lowercase().contains(&filter)
                || pane.current_path.to_lowercase().contains(&filter)
                || pane.target.to_lowercase().contains(&filter)
            {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

pub fn sort_panes(panes: &mut [Pane]) {
    panes.sort_by(|left, right| {
        left.display_title()
            .to_lowercase()
            .cmp(&right.display_title().to_lowercase())
            .then_with(|| left.current_path.cmp(&right.current_path))
            .then_with(|| left.target.cmp(&right.target))
    });
}

fn is_opencode_pane(pane: &Pane) -> bool {
    matches!(pane.current_command.as_str(), "opencode" | "opencode.exe")
}

fn jump_to_pane(target: &str) -> anyhow::Result<()> {
    let output = Command::new("tmux")
        .args(["switch-client", "-t", target])
        .output()
        .with_context(|| format!("failed to execute tmux switch-client for {target}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "tmux switch-client exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_tui(app: App) -> anyhow::Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).context("failed to enter alternate screen")?;
    let terminal = ratatui::init();

    let result = run_app(terminal, app);

    ratatui::restore();
    execute!(io::stdout(), LeaveAlternateScreen).context("failed to leave alternate screen")?;
    disable_raw_mode().context("failed to disable raw mode")?;

    result
}

fn run_app(mut terminal: DefaultTerminal, mut app: App) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if handle_key(key, &mut app)? => return Ok(()),
                Event::Key(_)
                | Event::Mouse(_)
                | Event::Resize(_, _)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Paste(_) => {}
            }
        }
    }
}

fn handle_key(key: KeyEvent, app: &mut App) -> anyhow::Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    match app.mode {
        InputMode::Normal => handle_normal_key(key, app),
        InputMode::Filtering => Ok(handle_filter_key(key, app)),
    }
}

fn handle_normal_key(key: KeyEvent, app: &mut App) -> anyhow::Result<bool> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Ok(true),
        KeyCode::Char('/') => {
            app.mode = InputMode::Filtering;
            app.message = "type to filter, Enter/Esc to finish".to_owned();
            Ok(false)
        }
        KeyCode::Char('r') => {
            app.refresh();
            Ok(false)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_down();
            Ok(false)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_up();
            Ok(false)
        }
        KeyCode::Enter => {
            if let Some(target) = app.selected_pane().map(|pane| pane.target.clone()) {
                jump_to_pane(&target)?;
                Ok(true)
            } else {
                app.message = "no pane selected".to_owned();
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn handle_filter_key(key: KeyEvent, app: &mut App) -> bool {
    match key.code {
        KeyCode::Enter | KeyCode::Esc => {
            app.mode = InputMode::Normal;
            app.message.clear();
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.apply_filter();
        }
        KeyCode::Char(char) => {
            app.filter.push(char);
            app.apply_filter();
        }
        _ => {}
    }
    false
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let mode = match app.mode {
        InputMode::Normal => "normal",
        InputMode::Filtering => "filter",
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "OpenCode Switchboard",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  mode: {mode}  filter: {}", app.filter)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let rows = app.filtered.iter().filter_map(|index| {
        app.panes.get(*index).map(|pane| {
            Row::new([
                Cell::from(pane.state.as_str()),
                Cell::from(pane.display_title().to_owned()),
                Cell::from(shorten_home(&pane.current_path)),
                Cell::from(pane.target.clone()),
            ])
        })
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Percentage(55),
            Constraint::Percentage(30),
            Constraint::Length(16),
        ],
    )
    .header(
        Row::new(["STATE", "TITLE", "DIR/WORKTREE", "TMUX"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::default().borders(Borders::ALL))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("» ");

    let mut table_state = TableState::default();
    if !app.filtered.is_empty() {
        table_state.select(Some(app.selected));
    }
    frame.render_stateful_widget(table, chunks[1], &mut table_state);

    let help = if app.message.is_empty() {
        "Enter jump  / filter  r refresh  q quit".to_owned()
    } else {
        app.message.clone()
    };
    frame.render_widget(Paragraph::new(help), chunks[2]);
}

fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path == home => "~".to_owned(),
        Ok(home) => path
            .strip_prefix(&(home + "/"))
            .map_or_else(|| path.to_owned(), |rest| format!("~/{rest}")),
        Err(_) => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn pane(title: &str, path: &str, target: &str) -> Pane {
        Pane {
            target: target.to_owned(),
            pid: 42,
            current_path: path.to_owned(),
            current_command: "opencode".to_owned(),
            pane_title: title.to_owned(),
            state: PaneState::Live,
        }
    }

    #[test]
    fn parses_tmux_pane_line() {
        let parsed = parse_tmux_pane_line(
            "notes:0.1\t123\t/Users/john/coding/app\topencode\tOC | Build MVP",
        )
        .expect("line should parse");

        assert_eq!(parsed.target, "notes:0.1");
        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.current_path, "/Users/john/coding/app");
        assert_eq!(parsed.current_command, "opencode");
        assert_eq!(parsed.display_title(), "Build MVP");
    }

    #[test]
    fn rejects_malformed_tmux_pane_line() {
        assert!(parse_tmux_pane_line("notes:0.1\tnot-a-pid\t/path\topencode\ttitle").is_none());
        assert!(parse_tmux_pane_line("notes:0.1\t123").is_none());
    }

    #[test]
    fn filters_by_title_path_or_target_case_insensitively() {
        let panes = vec![
            pane(
                "OC | Security Review",
                "/Users/john/coding/wormhole",
                "wh:1.0",
            ),
            pane(
                "OC | PRD implementation",
                "/Users/john/coding/opencode-switchboard",
                "tools:2.0",
            ),
        ];

        assert_eq!(filter_panes(&panes, "security"), vec![0]);
        assert_eq!(filter_panes(&panes, "SWITCHBOARD"), vec![1]);
        assert_eq!(filter_panes(&panes, "tools"), vec![1]);
        assert_eq!(filter_panes(&panes, ""), vec![0, 1]);
    }

    #[test]
    fn app_can_start_with_prepopulated_filter() {
        let panes = vec![
            pane(
                "OC | Wormhole review",
                "/Users/john/coding/wormhole",
                "wh:1.0",
            ),
            pane(
                "OC | PRD implementation",
                "/Users/john/coding/opencode-switchboard",
                "tools:2.0",
            ),
        ];

        let app = App::with_filter(panes, "wormhole".to_owned());

        assert_eq!(app.filter, "wormhole");
        assert_eq!(app.filtered, vec![0]);
    }

    #[test]
    fn parses_search_flag_forms() {
        assert_eq!(
            Options::parse([OsString::from("-s"), OsString::from("wormhole")]).unwrap(),
            Options {
                search: Some("wormhole".to_owned())
            }
        );
        assert_eq!(
            Options::parse([OsString::from("--search"), OsString::from("wormhole")]).unwrap(),
            Options {
                search: Some("wormhole".to_owned())
            }
        );
        assert_eq!(
            Options::parse([OsString::from("-s=wormhole")]).unwrap(),
            Options {
                search: Some("wormhole".to_owned())
            }
        );
        assert_eq!(
            Options::parse([OsString::from("--search=wormhole")]).unwrap(),
            Options {
                search: Some("wormhole".to_owned())
            }
        );
    }

    #[test]
    fn rejects_missing_search_phrase() {
        assert!(Options::parse([OsString::from("-s")]).is_err());
    }

    #[test]
    fn sorts_by_display_title_then_path_then_target() {
        let mut panes = vec![
            pane("OC | Zebra", "/b", "s:0.1"),
            pane("OC | alpha", "/c", "s:0.2"),
            pane("OC | Alpha", "/a", "s:0.3"),
        ];

        sort_panes(&mut panes);

        assert_eq!(panes[0].target, "s:0.3");
        assert_eq!(panes[1].target, "s:0.2");
        assert_eq!(panes[2].target, "s:0.1");
    }

    #[test]
    fn identifies_opencode_commands() {
        let mut candidate = pane("title", "/path", "s:0.0");
        assert!(is_opencode_pane(&candidate));

        candidate.current_command = "opencode.exe".to_owned();
        assert!(is_opencode_pane(&candidate));

        candidate.current_command = "zsh".to_owned();
        assert!(!is_opencode_pane(&candidate));
    }

    #[test]
    fn ordering_regression_is_total() {
        let mut panes = vec![
            pane("same", "/same", "b:0.0"),
            pane("same", "/same", "a:0.0"),
        ];

        sort_panes(&mut panes);

        assert_eq!(panes[0].target.cmp(&panes[1].target), Ordering::Less);
    }
}
