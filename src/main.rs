use std::{
    cmp::Ordering,
    error::Error,
    fs,
    fs::File,
    io::{self, BufReader},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use rand::prelude::*;
use regex::Regex;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

/// Focus options for our three boxes.
#[derive(PartialEq)]
enum Focus {
    Vinyl,
    Albums,
    SongList,
}

/// Application modes.
#[derive(PartialEq)]
enum AppState {
    Browsing, // No album playing.
    Playing,  // An album is inserted.
    SongList, // The song list view is active.
}

/// Simple vinyl simulator.
///
/// Usage:
///     levari -d /path/to/your/artists
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Data directory containing album directories (searched recursively)
    #[arg(short, long)]
    d: PathBuf,
}

/// A song.
#[derive(Debug)]
struct Song {
    title: String,
    duration: u64, // seconds (computed from file size)
    path: PathBuf,
}

/// An album.
#[derive(Debug)]
struct Album {
    name: String,
    path: PathBuf,
    cover: Option<PathBuf>,
    songs: Vec<Song>,
    bookmarked: bool,
}

/// Application state.
struct App {
    albums: Vec<Album>,
    state: AppState,
    selected_index: usize,          // Selected album in Albums box.
    playing_album: Option<usize>,   // Which album is inserted (if any).
    playback_start: Option<Instant>,// Start time for current song.
    pause_duration: Duration,       // Total paused time for current song.
    paused: bool,
    pause_start: Option<Instant>,
    album_list_state: ListState,
    song_list_state: ListState,
    current_sink: Option<Sink>,
    current_message: Option<String>,// Shows warnings or status in the footer.
    volume: f32,
    current_song_index: usize,      // Which song is currently playing.
    focus: Focus,                   // Which box has focus.
    title_phrase: String,           // Randomized phrase for the header only.
    playback_speed: f32,            // Simulated playback speed (RPM).

    pending_g: bool,                // For handling 'gg' in a vim-like way.
    message_time: Option<Instant>,  // Timestamp when current_message was set.
}

impl App {
    fn new(albums: Vec<Album>) -> App {
        let mut album_state = ListState::default();
        if !albums.is_empty() {
            album_state.select(Some(0));
        }
        let mut song_state = ListState::default();
        song_state.select(Some(0));

        let phrases = [
            "Spinning Vinyl...",
            "Warm Crackle Vibes",
            "Analog Dreams",
            "Groove On!",
        ];
        let mut rng = rand::thread_rng();
        let title_phrase = (&phrases[..]).choose(&mut rng).unwrap().to_string();

        App {
            albums,
            state: AppState::Browsing,
            selected_index: 0,
            playing_album: None,
            playback_start: None,
            pause_duration: Duration::from_secs(0),
            paused: false,
            pause_start: None,
            album_list_state: album_state,
            song_list_state: song_state,
            current_sink: None,
            current_message: None,
            volume: 1.0,
            current_song_index: 0,
            focus: Focus::Albums,
            title_phrase,
            playback_speed: 33.33,
            pending_g: false,
            message_time: None,
        }
    }

    // Helper to set a transient (decaying) message in the footer.
    fn set_message(&mut self, msg: impl Into<String>) {
        self.current_message = Some(msg.into());
        self.message_time = Some(Instant::now());
    }

    // Basic album navigation
    fn next_album(&mut self) {
        if self.albums.is_empty() {
            return;
        }
        if self.selected_index < self.albums.len() - 1 {
            self.selected_index += 1;
            self.album_list_state.select(Some(self.selected_index));
        }
    }
    fn previous_album(&mut self) {
        if self.albums.is_empty() {
            return;
        }
        if self.selected_index == 0 {
            // If we're at the top, jump focus to Vinyl (optional convenience).
            self.focus = Focus::Vinyl;
        } else {
            self.selected_index -= 1;
            self.album_list_state.select(Some(self.selected_index));
        }
    }
    fn go_to_top_album(&mut self) {
        if !self.albums.is_empty() {
            self.selected_index = 0;
            self.album_list_state.select(Some(0));
        }
    }
    fn go_to_bottom_album(&mut self) {
        if !self.albums.is_empty() {
            let last = self.albums.len() - 1;
            self.selected_index = last;
            self.album_list_state.select(Some(last));
        }
    }
    fn half_page_down_album(&mut self) {
        if self.albums.is_empty() {
            return;
        }
        let half = std::cmp::max(1, self.albums.len() / 2);
        let next_idx = std::cmp::min(self.selected_index + half, self.albums.len() - 1);
        self.selected_index = next_idx;
        self.album_list_state.select(Some(next_idx));
    }
    fn half_page_up_album(&mut self) {
        if self.albums.is_empty() {
            return;
        }
        let half = std::cmp::max(1, self.albums.len() / 2);
        let next_idx = self.selected_index.saturating_sub(half);
        self.selected_index = next_idx;
        self.album_list_state.select(Some(next_idx));
    }

    // Song list navigation
    fn next_song(&mut self) {
        if self.focus != Focus::SongList {
            return;
        }
        let album = &self.albums[self.selected_index];
        if album.songs.is_empty() {
            return;
        }
        let cur = self.song_list_state.selected().unwrap_or(0);
        let next = if cur + 1 >= album.songs.len() { cur } else { cur + 1 };
        self.song_list_state.select(Some(next));
    }
    fn previous_song(&mut self) {
        if self.focus != Focus::SongList {
            return;
        }
        let album = &self.albums[self.selected_index];
        if album.songs.is_empty() {
            return;
        }
        let cur = self.song_list_state.selected().unwrap_or(0);
        let prev = if cur == 0 { cur } else { cur - 1 };
        self.song_list_state.select(Some(prev));
    }

    /// Toggle "bookmark" on the currently selected album (if focus is Albums).
    fn toggle_bookmark(&mut self) {
        if self.focus != Focus::Albums {
            return;
        }
        if self.albums.is_empty() {
            return;
        }
        let album = &mut self.albums[self.selected_index];
        album.bookmarked = !album.bookmarked;
        let msg = if album.bookmarked {
            format!("Bookmarked '{}'", album.name)
        } else {
            format!("Removed bookmark '{}'", album.name)
        };
        self.set_message(msg);
    }

    /// Jump to the next bookmarked album (wrap around).
    fn next_bookmark(&mut self) {
        if self.focus != Focus::Albums || self.albums.is_empty() {
            return;
        }
        let start = self.selected_index;
        let len = self.albums.len();
        for offset in 1..=len {
            let i = (start + offset) % len;
            if self.albums[i].bookmarked {
                self.selected_index = i;
                self.album_list_state.select(Some(i));
                self.set_message(format!("Jumped to bookmarked album '{}'", self.albums[i].name));
                break;
            }
        }
    }

    /// Jump to the previous bookmarked album (wrap around).
    fn prev_bookmark(&mut self) {
        if self.focus != Focus::Albums || self.albums.is_empty() {
            return;
        }
        let start = self.selected_index;
        let len = self.albums.len();
        for offset in 1..=len {
            let i = (start + len - offset) % len;
            if self.albums[i].bookmarked {
                self.selected_index = i;
                self.album_list_state.select(Some(i));
                self.set_message(format!("Jumped to bookmarked album '{}'", self.albums[i].name));
                break;
            }
        }
    }

    /// Jump to the currently playing album (if any).
    fn jump_to_playing_album(&mut self) {
        if let Some(p) = self.playing_album {
            self.selected_index = p;
            self.album_list_state.select(Some(p));
            self.focus = Focus::Albums;
            self.set_message(format!("Jumped to playing album '{}'", self.albums[p].name));
        } else {
            self.set_message("No album is playing!");
        }
    }

    /// Set focus explicitly.
    fn set_focus(&mut self, new_focus: Focus) {
        self.focus = new_focus;
    }

    /// Handle SHIFT keys to switch focus or do other actions (Shift+H/J/K/L/G).
    fn handle_shift_key(&mut self, key: char) {
        match key {
            'J' => {
                // SHIFT+J => from Vinyl to Albums
                if self.focus == Focus::Vinyl {
                    self.focus = Focus::Albums;
                }
            }
            'K' => {
                // SHIFT+K => from Albums or SongList to Vinyl
                if self.focus == Focus::Albums || self.focus == Focus::SongList {
                    self.focus = Focus::Vinyl;
                }
            }
            'L' => {
                // SHIFT+L => from Albums to SongList, but only if the album is inserted
                if self.focus == Focus::Albums {
                    match self.playing_album {
                        Some(idx) if idx == self.selected_index => {
                            self.focus = Focus::SongList;
                        }
                        _ => {
                            self.set_message("That album is not inserted. Press SPACE to insert.");
                        }
                    }
                }
            }
            'H' => {
                // SHIFT+H => from SongList to Albums
                if self.focus == Focus::SongList {
                    self.focus = Focus::Albums;
                }
            }
            'G' => {
                // SHIFT+G => bottom of album list
                if self.focus == Focus::Albums {
                    self.go_to_bottom_album();
                }
            }
            _ => {}
        }
    }

    /// Insert the selected album and start playing from the beginning.
    /// If another album is already playing, eject it first automatically.
    fn insert_album(&mut self, stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        if let Some(current_play_idx) = self.playing_album {
            if current_play_idx != self.selected_index {
                // We are swapping albums automatically.
                self.eject_current_album();
            } else {
                // If it's the same album, do nothing special (already playing).
                // You could decide whether to "restart" if you want.
                return Ok(());
            }
        }
        self.playing_album = Some(self.selected_index);
        self.state = AppState::Playing;

        // Build sink
        let sink = Sink::try_new(stream_handle)?;

        // Append all songs
        let album = &self.albums[self.selected_index];
        for song in &album.songs {
            let file = File::open(&song.path)?;
            let source = Decoder::new(BufReader::new(file))?;
            sink.append(source);
        }
        sink.set_volume(self.volume);

        // Let it play immediately
        sink.play();

        // Reset timing state
        self.playback_start = Some(Instant::now());
        self.pause_duration = Duration::from_secs(0);
        self.paused = false;
        self.pause_start = None;
        self.current_song_index = 0;
        self.song_list_state.select(Some(0));

        // Keep the new sink
        self.current_sink = Some(sink);

        self.set_message(format!("Album '{}' inserted and playing.", album.name));
        Ok(())
    }

    /// Eject whatever album is currently playing.
    fn eject_current_album(&mut self) {
        if let Some(idx) = self.playing_album {
            let name = self.albums[idx].name.clone();
            self.state = AppState::Browsing;
            self.playing_album = None;
            self.playback_start = None;
            self.pause_duration = Duration::from_secs(0);
            self.paused = false;
            self.pause_start = None;
            self.current_sink.take();
            self.set_message(format!("Album '{}' ejected.", name));
        }
    }

    /// The Space key main behavior:
    /// - In Albums focus: Insert the selected album (auto-play). If a different album is already
    ///   playing, we swap automatically.
    /// - In Vinyl focus: toggle play/pause.
    /// - In Song List focus: Skip to the selected song. If a different album was playing,
    ///   automatically swap to the newly selected album first.
    fn space_action(&mut self, stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        match self.focus {
            Focus::Albums => {
                self.insert_album(stream_handle)?;
            }
            Focus::Vinyl => {
                self.toggle_pause();
            }
            Focus::SongList => {
                self.skip_to_song(stream_handle)?;
            }
        }
        Ok(())
    }

    /// Skip to the selected song in the current album. If the album is not inserted
    /// or a different one is playing, auto-insert (swap) first, then skip.
    fn skip_to_song(&mut self, stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        // If the album is not inserted or is different, auto-insert it:
        if self.playing_album != Some(self.selected_index) {
            self.insert_album(stream_handle)?;
        }
        let album = &self.albums[self.selected_index];
        let song_index = self.song_list_state.selected().unwrap_or(0);
        if song_index >= album.songs.len() {
            return Ok(());
        }
        let song_title = album.songs[song_index].title.clone();

        // Build a fresh sink, skipping directly to that song:
        let sink = Sink::try_new(stream_handle)?;
        // Append only from the chosen song onward
        for song in album.songs.iter().skip(song_index) {
            let file = File::open(&song.path)?;
            let source = Decoder::new(BufReader::new(file))?;
            sink.append(source);
        }
        sink.set_volume(self.volume);
        sink.play();

        // Reset playback state
        self.current_sink = Some(sink);
        self.state = AppState::Playing;
        self.playback_start = Some(Instant::now());
        self.pause_duration = Duration::from_secs(0);
        self.paused = false;
        self.pause_start = None;
        self.current_song_index = song_index;

        self.set_message(format!("Skipped to: '{}'", song_title));
        Ok(())
    }

    fn increase_volume(&mut self) {
        self.volume = (self.volume + 0.1).min(2.0);
        if let Some(ref sink) = self.current_sink {
            sink.set_volume(self.volume);
        }
        self.set_message(format!("Volume: {}%", (self.volume * 100.0) as u32));
    }
    fn decrease_volume(&mut self) {
        self.volume = (self.volume - 0.1).max(0.0);
        if let Some(ref sink) = self.current_sink {
            sink.set_volume(self.volume);
        }
        self.set_message(format!("Volume: {}%", (self.volume * 100.0) as u32));
    }

    fn increase_speed(&mut self) {
        self.playback_speed = (self.playback_speed + 1.0).min(78.0);
        self.set_message(format!("Speed: {:.2} RPM", self.playback_speed));
    }
    fn decrease_speed(&mut self) {
        self.playback_speed = (self.playback_speed - 1.0).max(33.33);
        self.set_message(format!("Speed: {:.2} RPM", self.playback_speed));
    }

    fn toggle_pause(&mut self) {
        if let Some(ref sink) = self.current_sink {
            if self.paused {
                sink.play();
                if let Some(pause_start) = self.pause_start {
                    let paused_time = pause_start.elapsed();
                    self.pause_duration += paused_time;
                }
                self.paused = false;
                self.pause_start = None;
                self.set_message("Playing...");
            } else {
                sink.pause();
                self.paused = true;
                self.pause_start = Some(Instant::now());
                self.set_message("Paused.");
            }
        } else {
            self.set_message("No album is inserted yet.");
        }
    }

    fn on_tick(&mut self) {
        // Decay messages after ~3 seconds
        if let Some(ts) = self.message_time {
            let timeout = Duration::from_secs(3);
            if ts.elapsed() >= timeout {
                self.current_message = None;
                self.message_time = None;
            }
        }
    }
}

/// Render the Vinyl Player box.
fn render_vinyl_player(app: &App) -> String {
    if let Some(play_idx) = app.playing_album {
        let album = &app.albums[play_idx];
        let cumulative: u64 = album
            .songs
            .iter()
            .take(app.current_song_index)
            .map(|s| s.duration)
            .sum();
        let current_elapsed = if let Some(start) = app.playback_start {
            let raw = start.elapsed();
            let effective = if app.paused {
                // If paused, measure only up to pause_start
                app.pause_start.unwrap_or(start).saturating_duration_since(start)
            } else {
                raw
            }
            .saturating_sub(app.pause_duration);
            effective.as_secs()
        } else {
            0
        };
        let total_elapsed = cumulative + current_elapsed;
        let minutes = total_elapsed / 60;
        let seconds = total_elapsed % 60;
        let status = if app.paused { "Paused" } else { "Playing" };
        format!(
            "Album: {}\nPath: {}\n\nElapsed: {:02}:{:02}\nVolume: {}%\nStatus: {}",
            album.name,
            album.path.display(),
            minutes,
            seconds,
            (app.volume * 100.0) as u32,
            status
        )
    } else {
        "No album playing".to_string()
    }
}

/// Draw the UI.
fn ui<B: tui::backend::Backend>(f: &mut tui::Frame<B>, app: &mut App) {
    // Layout: header, main, footer.
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Main
            Constraint::Length(3), // Footer
        ])
        .split(f.size());

    // --- Header ---
    let header_text = Spans::from(vec![
        Span::styled(
            "Levari",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" - "),
        Span::styled(&app.title_phrase, Style::default().fg(Color::Yellow)),
    ]);
    let header = Paragraph::new(header_text).block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, main_chunks[0]);

    // --- Main area ---
    // Split main area vertically: top row (Vinyl Player) and bottom row (Albums, Song List).
    let main_vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)].as_ref())
        .split(main_chunks[1]);

    // Top row: Vinyl Player box.
    let vinyl_block = if app.focus == Focus::Vinyl {
        Block::default()
            .borders(Borders::ALL)
            .title("Vinyl Player")
            .border_style(Style::default().fg(Color::Green))
    } else {
        Block::default().borders(Borders::ALL).title("Vinyl Player")
    };
    let vinyl_text = render_vinyl_player(app);
    let vinyl_paragraph = Paragraph::new(vinyl_text).block(vinyl_block);
    f.render_widget(vinyl_paragraph, main_vertical[0]);

    // Bottom row: split horizontally -> Albums (left) and Song List (right).
    let bottom_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)].as_ref())
        .split(main_vertical[1]);

    // Albums box.
    let album_block = if app.focus == Focus::Albums {
        Block::default()
            .borders(Borders::ALL)
            .title("Albums")
            .border_style(Style::default().fg(Color::Green))
    } else {
        Block::default().borders(Borders::ALL).title("Albums")
    };
    let album_items: Vec<ListItem> = app
        .albums
        .iter()
        .enumerate()
        .map(|(i, album)| {
            let mut name = album.name.clone();
            if album.bookmarked {
                name.push_str(" [*]");
            }
            if app.playing_album == Some(i) {
                name.push_str(" [INSERTED]");
                ListItem::new(Spans::from(Span::styled(
                    name,
                    Style::default().fg(Color::Blue),
                )))
            } else {
                ListItem::new(Spans::from(Span::raw(name)))
            }
        })
        .collect();
    let albums_list = List::new(album_items)
        .block(album_block)
        .highlight_style(Style::default().fg(Color::Yellow))
        .highlight_symbol(">> ");
    f.render_stateful_widget(albums_list, bottom_columns[0], &mut app.album_list_state);

    // Song List box.
    let album_for_songs = &app.albums[app.selected_index];
    let song_block = if app.focus == Focus::SongList {
        Block::default()
            .borders(Borders::ALL)
            .title("Song List")
            .border_style(Style::default().fg(Color::Green))
    } else {
        Block::default().borders(Borders::ALL).title("Song List")
    };
    let mut cum = 0;
    let song_items: Vec<ListItem> = album_for_songs
        .songs
        .iter()
        .enumerate()
        .map(|(i, song)| {
            let start_time = cum;
            cum += song.duration;
            let minutes = start_time / 60;
            let seconds = start_time % 60;
            let mut line = format!("{} [{:02}:{:02}]", song.title, minutes, seconds);
            if app.focus == Focus::SongList && Some(i) == app.song_list_state.selected() {
                line = format!("> {}", line);
            }
            ListItem::new(line)
        })
        .collect();
    let songs_list = List::new(song_items)
        .block(song_block)
        .highlight_style(Style::default().fg(Color::Cyan));
    f.render_widget(songs_list, bottom_columns[1]);

    // --- Footer ---
    let footer_text = if let Some(ref msg) = app.current_message {
        Spans::from(vec![Span::raw(msg)])
    } else {
        Spans::from(vec![Span::raw(
            "Controls: Insert - 'Space', Navigate - 'h/j/k/l', Change Focus - 'SHIFT+H/J/K/L', Mark - 'm', Jump Marks - 'n', Jump Playing - 'p', Volume: '+/-', Quit - 'q'",
        )])
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, main_chunks[2]);
}

/// Recursively load all albums from the given directory.
fn load_albums(dir: &Path) -> Result<Vec<Album>, Box<dyn Error>> {
    let mut albums = Vec::new();
    let album_candidate = load_album(dir)?;
    // If we detect it has either a cover or at least one song, treat as an album,
    // else dive deeper.
    if album_candidate.cover.is_some() || !album_candidate.songs.is_empty() {
        albums.push(album_candidate);
    } else {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                albums.extend(load_albums(&path)?);
            }
        }
    }
    Ok(albums)
}

/// Load a single album’s info (songs + optional cover).
fn load_album(dir: &Path) -> Result<Album, Box<dyn Error>> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown Album")
        .to_string();
    let mut cover = None;
    // Find a “cover.*” file
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                if fname.to_lowercase().starts_with("cover.") {
                    cover = Some(path.clone());
                    break;
                }
            }
        }
    }
    // Gather songs
    let mut songs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext = ext.to_lowercase();
                if ["mp3", "flac", "wav", "ogg"].contains(&ext.as_str()) {
                    let song_title = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Unknown Song")
                        .to_string();
                    let metadata = fs::metadata(&path)?;
                    let file_size = metadata.len();
                    // Rough guess at duration from file size
                    let duration = if file_size > 0 {
                        (file_size as f64 / 40_000.0).round() as u64
                    } else {
                        0
                    };
                    songs.push(Song {
                        title: song_title,
                        duration,
                        path: path.clone(),
                    });
                }
            }
        }
    }
    songs.sort_by(natural_order);
    Ok(Album {
        name,
        path: dir.to_path_buf(),
        cover,
        songs,
        bookmarked: false,
    })
}

fn natural_order(a: &Song, b: &Song) -> Ordering {
    let re = Regex::new(r"^(?P<prefix>[A-Za-z]*)(?P<num>\d+)").unwrap();
    let a_caps = re.captures(&a.title);
    let b_caps = re.captures(&b.title);
    match (a_caps, b_caps) {
        (Some(a_caps), Some(b_caps)) => {
            let a_prefix = a_caps.name("prefix").map(|m| m.as_str()).unwrap_or("");
            let b_prefix = b_caps.name("prefix").map(|m| m.as_str()).unwrap_or("");
            match a_prefix.cmp(b_prefix) {
                Ordering::Equal => {
                    let a_num = a_caps
                        .name("num")
                        .and_then(|m| m.as_str().parse::<u64>().ok())
                        .unwrap_or(0);
                    let b_num = b_caps
                        .name("num")
                        .and_then(|m| m.as_str().parse::<u64>().ok())
                        .unwrap_or(0);
                    a_num.cmp(&b_num)
                }
                other => other,
            }
        }
        _ => a.title.cmp(&b.title),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let mut albums = load_albums(&args.d)?;
    if albums.is_empty() {
        eprintln!("No albums found in {}", args.d.display());
        return Ok(());
    }

    // Shuffle for a bit of randomness in the Albums order.
    let mut rng = rand::thread_rng();
    albums.shuffle(&mut rng);

    let mut app = App::new(albums);
    let (_stream, stream_handle) = OutputStream::try_default()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let CEvent::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,

                    // SHIFT combos:
                    KeyCode::Char(c) if c.is_ascii_uppercase() => {
                        app.handle_shift_key(c);
                        app.pending_g = false;
                    }

                    // Navigation: hjkl
                    KeyCode::Char('j') => {
                        match app.focus {
                            Focus::Vinyl => app.set_focus(Focus::Albums),
                            Focus::Albums => app.next_album(),
                            Focus::SongList => app.next_song(),
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('k') => {
                        match app.focus {
                            Focus::Albums => app.previous_album(),
                            Focus::SongList => app.previous_song(),
                            _ => {}
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('h') => {
                        if app.focus == Focus::SongList {
                            // same as SHIFT+H basically
                            app.set_focus(Focus::Albums);
                        } else if app.focus == Focus::Vinyl {
                            // Eject if in Vinyl focus
                            app.eject_current_album();
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('l') => {
                        // from Albums to SongList (only if inserted)
                        if app.focus == Focus::Albums {
                            match app.playing_album {
                                Some(idx) if idx == app.selected_index => {
                                    app.set_focus(Focus::SongList);
                                }
                                _ => {
                                    app.set_message("That album is not inserted. Press SPACE to insert.");
                                }
                            }
                        }
                        app.pending_g = false;
                    }

                    // Space key
                    KeyCode::Char(' ') => {
                        if let Err(e) = app.space_action(&stream_handle) {
                            eprintln!("Error during space action: {}", e);
                        }
                        app.pending_g = false;
                    }

                    // Volume/speed
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        app.increase_volume();
                        app.pending_g = false;
                    }
                    KeyCode::Char('-') => {
                        app.decrease_volume();
                        app.pending_g = false;
                    }
                    KeyCode::Char('>') => {
                        app.increase_speed();
                        app.pending_g = false;
                    }
                    KeyCode::Char('<') => {
                        app.decrease_speed();
                        app.pending_g = false;
                    }

                    // Bookmarking
                    KeyCode::Char('m') => {
                        app.toggle_bookmark();
                        app.pending_g = false;
                    }
                    KeyCode::Char('n') => {
                        app.next_bookmark();
                        app.pending_g = false;
                    }
                    KeyCode::Char('N') => {
                        app.prev_bookmark();
                        app.pending_g = false;
                    }

                    // Jump to playing
                    KeyCode::Char('p') => {
                        app.jump_to_playing_album();
                        app.pending_g = false;
                    }

                    // Vim-like 'gg'
                    KeyCode::Char('g') => {
                        if app.focus == Focus::Albums {
                            if !app.pending_g {
                                // first 'g'
                                app.pending_g = true;
                            } else {
                                // second 'g'
                                app.go_to_top_album();
                                app.pending_g = false;
                            }
                        } else {
                            app.pending_g = false;
                        }
                    }
                    // SHIFT+G is handled above, so do nothing here
                    KeyCode::Char('G') => {
                        app.pending_g = false;
                    }

                    // Ctrl+d / Ctrl+u
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.focus == Focus::Albums {
                            app.half_page_down_album();
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.focus == Focus::Albums {
                            app.half_page_up_album();
                        }
                        app.pending_g = false;
                    }

                    _ => {
                        // Reset pending_g for any other key
                        app.pending_g = false;
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

