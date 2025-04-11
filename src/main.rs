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
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

#[derive(PartialEq)]
enum Focus {
    Vinyl,
    Albums,
    SongList,
}

#[derive(PartialEq)]
enum AppState {
    Browsing,
    Playing,
    SongList,
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 'd', long = "datadir")]
    datadir: PathBuf,
}

#[derive(Debug)]
struct Song {
    title: String,
    duration: u64,
    path: PathBuf,
}

#[derive(Debug)]
struct Album {
    name: String,
    path: PathBuf,
    cover: Option<PathBuf>,
    songs: Vec<Song>,
    bookmarked: bool,
}

struct App {
    albums: Vec<Album>,
    state: AppState,
    selected_index: usize,
    playing_album: Option<usize>,
    playback_start: Option<Instant>,
    pause_duration: Duration,
    paused: bool,
    pause_start: Option<Instant>,
    album_list_state: ListState,
    song_list_state: ListState,
    current_sink: Option<Sink>,
    current_message: Option<String>,
    volume: f32,
    current_song_index: usize,
    focus: Focus,
    title_phrase: String,
    playback_speed: f32,
    pending_g: bool,
    message_time: Option<Instant>,
}

impl App {
    fn new(albums: Vec<Album>) -> Self {
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
            "Retro Beats",
            "Sonic Nostalgia",
            "Vinyl Vibes",
            "Spin It to Win It",
        ];
        let mut rng = rand::thread_rng();
        let title_phrase = phrases.choose(&mut rng).unwrap().to_string();
        Self {
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
            volume: 0.25, // initial volume 25%
            current_song_index: 0,
            focus: Focus::Albums,
            title_phrase,
            playback_speed: 33.0,
            pending_g: false,
            message_time: None,
        }
    }

    fn set_message(&mut self, msg: impl Into<String>) {
        self.current_message = Some(msg.into());
        self.message_time = Some(Instant::now());
    }

    // --- Navigation Methods ---
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
        let half = self.albums.len() / 2;
        let next_idx = std::cmp::min(self.selected_index + half.max(1), self.albums.len() - 1);
        self.selected_index = next_idx;
        self.album_list_state.select(Some(next_idx));
    }
    fn half_page_up_album(&mut self) {
        if self.albums.is_empty() {
            return;
        }
        let half = self.albums.len() / 2;
        let next_idx = self.selected_index.saturating_sub(half.max(1));
        self.selected_index = next_idx;
        self.album_list_state.select(Some(next_idx));
    }

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

    fn toggle_bookmark(&mut self) {
        if self.focus != Focus::Albums || self.albums.is_empty() {
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

    fn set_focus(&mut self, new_focus: Focus) {
        self.focus = new_focus;
    }

    fn handle_shift_key(&mut self, key: char) {
        match key {
            'J' if self.focus == Focus::Vinyl => self.focus = Focus::Albums,
            'K' if self.focus == Focus::Albums || self.focus == Focus::SongList => self.focus = Focus::Vinyl,
            'L' if self.focus == Focus::Albums => {
                if let Some(idx) = self.playing_album {
                    if idx == self.selected_index {
                        self.focus = Focus::SongList;
                    } else {
                        self.set_message("That album is not inserted. Press ENTER to insert.");
                    }
                } else {
                    self.set_message("That album is not inserted. Press ENTER to insert.");
                }
            }
            'H' if self.focus == Focus::SongList => self.focus = Focus::Albums,
            'G' if self.focus == Focus::Albums => self.go_to_bottom_album(),
            _ => {}
        }
    }

    // --- Helper Functions ---
    fn playback_factor(&self) -> f32 {
        self.playback_speed / 33.0
    }

    fn effective_elapsed(&self) -> f64 {
        if let Some(start) = self.playback_start {
            let raw = start.elapsed();
            let effective = if self.paused {
                self.pause_start.unwrap_or(start).saturating_duration_since(start)
            } else {
                raw
            }
            .saturating_sub(self.pause_duration);
            effective.as_secs_f64()
        } else {
            0.0
        }
    }

    fn create_album_sink(
        &self,
        stream_handle: &OutputStreamHandle,
        album: &Album,
        start_index: usize,
    ) -> Result<Sink, Box<dyn Error>> {
        let sink = Sink::try_new(stream_handle)?;
        let factor = self.playback_factor();
        for song in album.songs.iter().skip(start_index) {
            let file = File::open(&song.path)?;
            let source = Decoder::new(BufReader::new(file))?;
            sink.append(source.speed(factor));
        }
        sink.set_volume(self.volume);
        sink.play();
        Ok(sink)
    }

    // --- Player Actions ---
    fn insert_album(&mut self, stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        if let Some(current) = self.playing_album {
            if current != self.selected_index {
                self.eject_current_album();
            } else {
                return Ok(());
            }
        }
        self.playing_album = Some(self.selected_index);
        self.state = AppState::Playing;
        let album = &self.albums[self.selected_index];
        let sink = self.create_album_sink(stream_handle, album, 0)?;
        self.playback_start = Some(Instant::now());
        self.pause_duration = Duration::from_secs(0);
        self.paused = false;
        self.pause_start = None;
        self.current_song_index = 0;
        self.song_list_state.select(Some(0));
        self.current_sink = Some(sink);
        self.set_message(format!("Album '{}' inserted and playing.", album.name));
        Ok(())
    }

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

    // SPACE toggles pause
    fn space_action(&mut self, _stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        self.toggle_pause();
        Ok(())
    }

    // Always recreates the sink so that skipping starts at the selected song.
    fn skip_to_song(&mut self, stream_handle: &OutputStreamHandle) -> Result<(), Box<dyn Error>> {
        self.playing_album = Some(self.selected_index);
        let album = &self.albums[self.selected_index];
        let song_index = self.song_list_state.selected().unwrap_or(0);
        if song_index >= album.songs.len() {
            return Ok(());
        }
        let song_title = album.songs[song_index].title.clone();
        let sink = self.create_album_sink(stream_handle, album, song_index)?;
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

    fn increase_speed(&mut self, stream_handle: &OutputStreamHandle) {
        self.playback_speed = match self.playback_speed {
            33.0 => 45.0,
            45.0 => 78.0,
            78.0 => 78.0,
            _ => 33.0,
        };
        self.set_message(format!("Speed: {:.0} RPM", self.playback_speed));
        self.update_speed(stream_handle);
    }
    fn decrease_speed(&mut self, stream_handle: &OutputStreamHandle) {
        self.playback_speed = match self.playback_speed {
            78.0 => 45.0,
            45.0 => 33.0,
            33.0 => 33.0,
            _ => 33.0,
        };
        self.set_message(format!("Speed: {:.0} RPM", self.playback_speed));
        self.update_speed(stream_handle);
    }
    fn update_speed(&mut self, stream_handle: &OutputStreamHandle) {
        if let Some(current_album_idx) = self.playing_album {
            let album = &self.albums[current_album_idx];
            let song_index = self.current_song_index;
            let effective_elapsed = self.effective_elapsed();
            let cumulative: f64 = album.songs.iter().take(song_index).map(|s| s.duration as f64).sum();
            let offset_in_current = effective_elapsed - cumulative;
            let factor = self.playback_factor();
            let sink = Sink::try_new(stream_handle).unwrap();
            if song_index < album.songs.len() {
                let current_song = &album.songs[song_index];
                let file = File::open(&current_song.path).unwrap();
                let reader = BufReader::new(file);
                let decoder = Decoder::new(reader).unwrap();
                let current_source = decoder.skip_duration(Duration::from_secs_f64(offset_in_current));
                sink.append(current_source.speed(factor));
            }
            for song in album.songs.iter().skip(song_index + 1) {
                let file = File::open(&song.path).unwrap();
                let source = Decoder::new(BufReader::new(file)).unwrap();
                sink.append(source.speed(factor));
            }
            sink.set_volume(self.volume);
            sink.play();
            self.current_sink = Some(sink);
            let new_start = Instant::now() - Duration::from_secs_f64(effective_elapsed);
            self.playback_start = Some(new_start);
        }
    }

    fn increase_volume(&mut self) {
        self.volume = (self.volume + 0.01).min(2.0);
        if let Some(ref sink) = self.current_sink {
            sink.set_volume(self.volume);
        }
        self.set_message(format!("Volume: {}%", (self.volume * 100.0) as u32));
    }
    fn decrease_volume(&mut self) {
        self.volume = (self.volume - 0.01).max(0.0);
        if let Some(ref sink) = self.current_sink {
            sink.set_volume(self.volume);
        }
        self.set_message(format!("Volume: {}%", (self.volume * 100.0) as u32));
    }

    fn toggle_pause(&mut self) {
        if let Some(ref sink) = self.current_sink {
            if self.paused {
                sink.play();
                if let Some(pause_start) = self.pause_start {
                    self.pause_duration += pause_start.elapsed();
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
            self.set_message("No album is inserted yet. Press ENTER to insert.");
        }
    }

    fn on_tick(&mut self) {
        if let Some(ts) = self.message_time {
            if ts.elapsed() >= Duration::from_secs(3) {
                self.current_message = None;
                self.message_time = None;
            }
        }
    }
}

fn render_vinyl_player(app: &App) -> String {
    if let Some(play_idx) = app.playing_album {
        let album = &app.albums[play_idx];
        let cumulative: u64 = album.songs.iter().take(app.current_song_index).map(|s| s.duration).sum();
        let current_elapsed = if app.playback_start.is_some() {
            app.effective_elapsed() as u64
        } else {
            0
        };
        let total_elapsed = cumulative + current_elapsed;
        let minutes = total_elapsed / 60;
        let seconds = total_elapsed % 60;
        let status = if app.paused { "Paused" } else { "Playing" };
        format!(
            "Album: {}\nPath: {}\n\nElapsed: {:02}:{:02}\nVolume: {}%\nRPM: {:.0} RPM\nStatus: {}",
            album.name,
            album.path.display(),
            minutes,
            seconds,
            (app.volume * 100.0) as u32,
            app.playback_speed,
            status
        )
    } else {
        "No album playing".to_string()
    }
}

fn ui<B: tui::backend::Backend>(f: &mut tui::Frame<B>, app: &mut App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)].as_ref())
        .split(f.size());

    let header_text = Spans::from(vec![
        Span::styled("Levari", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" - "),
        Span::styled(&app.title_phrase, Style::default().fg(Color::Magenta)),
    ]);
    let header = Paragraph::new(header_text).block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, main_chunks[0]);

    let main_vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)].as_ref())
        .split(main_chunks[1]);
    let player_border = if app.focus == Focus::Vinyl { Color::Magenta } else { Color::Yellow };
    let vinyl_block = Block::default()
        .borders(Borders::ALL)
        .title("Player")
        .border_style(Style::default().fg(player_border));
    let vinyl_text = render_vinyl_player(app);
    let vinyl_paragraph = Paragraph::new(vinyl_text).block(vinyl_block);
    f.render_widget(vinyl_paragraph, main_vertical[0]);

    let bottom_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)].as_ref())
        .split(main_vertical[1]);
    let album_border = if app.focus == Focus::Albums { Color::Magenta } else { Color::Yellow };
    let album_block = Block::default()
        .borders(Borders::ALL)
        .title("Shelf")
        .border_style(Style::default().fg(album_border));
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
                ListItem::new(Spans::from(Span::styled(name, Style::default().fg(Color::Magenta))))
            } else {
                ListItem::new(Spans::from(Span::raw(name)))
            }
        })
        .collect();
    let albums_list = List::new(album_items)
        .block(album_block)
        .highlight_style(Style::default().fg(Color::Magenta))
        .highlight_symbol(">> ");
    f.render_stateful_widget(albums_list, bottom_columns[0], &mut app.album_list_state);

    let song_border = if app.focus == Focus::SongList { Color::Magenta } else { Color::Yellow };
    let album_for_songs = &app.albums[app.selected_index];
    let song_block = Block::default()
        .borders(Borders::ALL)
        .title("Backside")
        .border_style(Style::default().fg(song_border));
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
        .highlight_style(Style::default().fg(Color::Magenta));
    f.render_widget(songs_list, bottom_columns[1]);

    let footer_text = if let Some(ref msg) = app.current_message {
        Spans::from(vec![Span::raw(msg)])
    } else {
        Spans::from(vec![Span::raw("Space = Play/Pause  |  Enter = Insert/Eject/Skip  |  h/j/k/l = Navigate  |  Shift+H/J/K/L = Change Focus  |  m = Bookmark  |  n/N = Next/Prev Bookmark  |  +/- = Volume  |  >/< = Speed  |  q = Quit")])
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, main_chunks[2]);
}

fn load_albums(dir: &Path) -> Result<Vec<Album>, Box<dyn Error>> {
    let mut albums = Vec::new();
    let album_candidate = load_album(dir)?;
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

fn load_album(dir: &Path) -> Result<Album, Box<dyn Error>> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown Album")
        .to_string();
    let mut cover = None;
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
    let mut songs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ["mp3", "flac", "wav", "ogg"].contains(&ext.to_lowercase().as_str()) {
                    let song_title = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Unknown Song")
                        .to_string();
                    let metadata = fs::metadata(&path)?;
                    let file_size = metadata.len();
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
    let mut albums = load_albums(&args.datadir)?;
    if albums.is_empty() {
        eprintln!("No albums found in {}", args.datadir.display());
        return Ok(());
    }
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
                    KeyCode::Char('n') => {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::SHIFT)
                        {
                            app.prev_bookmark();
                        } else {
                            app.next_bookmark();
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('N') => {
                        app.prev_bookmark();
                        app.pending_g = false;
                    }
                    KeyCode::Char(c) if c.is_ascii_uppercase() && c != 'N' => {
                        app.handle_shift_key(c);
                        app.pending_g = false;
                    }
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
                            app.set_focus(Focus::Albums);
                        } else if app.focus == Focus::Vinyl {
                            app.eject_current_album();
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('l') => {
                        if app.focus == Focus::Albums {
                            app.set_focus(Focus::SongList);
                            let song_idx = if app.playing_album == Some(app.selected_index) {
                                app.current_song_index
                            } else {
                                0
                            };
                            app.song_list_state.select(Some(song_idx));
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char(' ') => {
                        if let Err(e) = app.space_action(&stream_handle) {
                            eprintln!("Error: {}", e);
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Enter => {
                        match app.focus {
                            Focus::Albums | Focus::Vinyl => {
                                if app.playing_album == Some(app.selected_index) {
                                    app.eject_current_album();
                                } else {
                                    if let Err(e) = app.insert_album(&stream_handle) {
                                        eprintln!("Error inserting album: {}", e);
                                    }
                                }
                            }
                            Focus::SongList => {
                                if let Err(e) = app.skip_to_song(&stream_handle) {
                                    eprintln!("Error skipping to song: {}", e);
                                }
                            }
                        }
                        app.pending_g = false;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        app.increase_volume();
                        app.pending_g = false;
                    }
                    KeyCode::Char('-') => {
                        app.decrease_volume();
                        app.pending_g = false;
                    }
                    KeyCode::Char('>') => {
                        app.increase_speed(&stream_handle);
                        app.pending_g = false;
                    }
                    KeyCode::Char('<') => {
                        app.decrease_speed(&stream_handle);
                        app.pending_g = false;
                    }
                    KeyCode::Char('m') => {
                        app.toggle_bookmark();
                        app.pending_g = false;
                    }
                    KeyCode::Char('p') => {
                        app.jump_to_playing_album();
                        app.pending_g = false;
                    }
                    KeyCode::Char('g') => {
                        if app.focus == Focus::Albums {
                            if !app.pending_g {
                                app.pending_g = true;
                            } else {
                                app.go_to_top_album();
                                app.pending_g = false;
                            }
                        } else {
                            app.pending_g = false;
                        }
                    }
                    KeyCode::Char('G') => {
                        app.pending_g = false;
                    }
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
