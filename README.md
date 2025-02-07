# 📀 Levari

> ⚠️  **Written by a Rust beginner relying heavily on AI**

Levari is a terminal-based, stateless music player that allows users to experience their music library in a new way. The application emulates the experience of a physical vinyl player. Your collection is always scuffed, like your record shelf, helping you discover hidden gems in your library. There are no playlists or queues; you must manually switch songs or listen to an entire album at once, as it was meant to be listened to.

## Usage

Run the script by passing it your music library path as an argument. It will be processed recursively:

```bash
nix run github:tupakkatapa/levari -- -d <path>
```

## Controls

- **Space:** Play/pause album, insert album, or skip to the selected song in the song list
- **h/j/k/l:** Navigate between items (e.g., albums and songs)
- **Shift + H/J/K/L:** Change focus between different interface sections
- **m:** Toggle a bookmark on the selected album
- **n/N**: Jump to the next/previous bookmarked album
- **p:** Jump to the currently playing album
- **+/-:** Increase/decrease volume
- **>/<:** Increase/decrease playback speed between 33, 45, or 78 RPM.
- **q:** Quit the application
