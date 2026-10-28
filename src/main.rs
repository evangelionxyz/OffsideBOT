use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command as StdCommand, Stdio},
    sync::{
        Arc, Condvar, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        channel::Message,
        gateway::{GatewayIntents, Ready},
        id::{ChannelId, GuildId, UserId},
        voice::VoiceState,
    },
    prelude::*,
};
use songbird::{
    Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit, TrackEvent,
    input::{
        AudioStream, Input, LiveInput,
        codecs::{get_codec_registry, get_probe},
        core::io::{MediaSource, ReadOnlySource},
    },
    tracks::{PlayMode, TrackHandle},
};
use tokio::{sync::Mutex, task::AbortHandle};

const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days
const MAX_CACHE_BYTES: u64 = 5 * 1024 * 1024 * 1024; // 5 GB

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SongInfo {
    id: String,
    url: String,
    title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEntry {
    id: String,
    title: String,
    url: String,
    file_name: String,
    size_bytes: u64,
    created_at: u64,
    last_accessed: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct CacheIndex {
    entries: HashMap<String, CacheEntry>,
    queries: HashMap<String, String>,
}

struct SongCache {
    dir: PathBuf,
    index_path: PathBuf,
    index: Mutex<CacheIndex>,
}

impl SongCache {
    fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = fs::create_dir_all(&dir);
        let index_path = dir.join("index.json");
        let index = fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str::<CacheIndex>(&s).ok())
            .unwrap_or_default();

        Self {
            dir,
            index_path,
            index: Mutex::new(index),
        }
    }

    fn normalize_query(q: &str) -> String {
        q.trim().to_lowercase()
    }

    fn sanitize_id(id: &str) -> String {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn file_path(&self, file_name: &str) -> PathBuf {
        self.dir.join(file_name)
    }

    async fn save_index_locked(&self, index: &CacheIndex) {
        if let Ok(json) = serde_json::to_string_pretty(index) {
            let tmp_path = self.dir.join("index.json.tmp");
            if fs::write(&tmp_path, json).is_ok() {
                let _ = fs::rename(&tmp_path, &self.index_path);
            }
        }
    }

    async fn enforce_limits(&self) {
        let mut index = self.index.lock().await;
        let now = now_unix_secs();
        let mut changed = false;

        // 1. Remove expired entries (> 7 days TTL) or missing files on disk
        let expired_ids: Vec<String> = index
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let path = self.file_path(&entry.file_name);
                let is_expired = now.saturating_sub(entry.created_at) > CACHE_TTL_SECS;
                if is_expired || !path.exists() {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in expired_ids {
            if let Some(entry) = index.entries.remove(&id) {
                let _ = fs::remove_file(self.file_path(&entry.file_name));
                changed = true;
            }
        }

        // Remove stale files in cache directory not tracked in index
        if let Ok(read_dir) = fs::read_dir(&self.dir) {
            for dir_entry in read_dir.flatten() {
                let path = dir_entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name == "index.json" || name == "index.json.tmp" || name.ends_with(".part") {
                    if name.ends_with(".part") {
                        // Clean up abandoned .part files older than 1 hour
                        if let Ok(meta) = dir_entry.metadata() {
                            if let Ok(modified) = meta.modified() {
                                if SystemTime::now()
                                    .duration_since(modified)
                                    .unwrap_or_default()
                                    > Duration::from_secs(3600)
                                {
                                    let _ = fs::remove_file(&path);
                                }
                            }
                        }
                    }
                    continue;
                }
                let tracked = index.entries.values().any(|e| e.file_name == name);
                if !tracked {
                    if let Ok(meta) = dir_entry.metadata() {
                        let age = meta
                            .modified()
                            .ok()
                            .and_then(|m| SystemTime::now().duration_since(m).ok())
                            .unwrap_or_default();
                        if age > Duration::from_secs(CACHE_TTL_SECS) {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }

        // 2. Enforce 5 GB max size using LRU (oldest last_accessed removed first)
        let mut total_bytes: u64 = index.entries.values().map(|e| e.size_bytes).sum();
        if total_bytes > MAX_CACHE_BYTES {
            let mut entries_by_age: Vec<(String, u64, String, u64)> = index
                .entries
                .values()
                .map(|e| {
                    (
                        e.id.clone(),
                        e.last_accessed,
                        e.file_name.clone(),
                        e.size_bytes,
                    )
                })
                .collect();
            entries_by_age.sort_by_key(|(_, accessed, _, _)| *accessed);

            for (id, _, file_name, size) in entries_by_age {
                if total_bytes <= MAX_CACHE_BYTES {
                    break;
                }
                let _ = fs::remove_file(self.file_path(&file_name));
                index.entries.remove(&id);
                total_bytes = total_bytes.saturating_sub(size);
                changed = true;
            }
        }

        if changed {
            let CacheIndex {
                ref entries,
                ref mut queries,
            } = *index;
            queries.retain(|_, id| entries.contains_key(id));
            self.save_index_locked(&index).await;
        }
    }

    async fn get_by_query(&self, query: &str) -> Option<(SongInfo, PathBuf)> {
        let key = Self::normalize_query(query);
        let mut index = self.index.lock().await;
        let now = now_unix_secs();

        let id = index.queries.get(&key)?.clone();
        let entry = index.entries.get_mut(&id)?;

        if now.saturating_sub(entry.created_at) > CACHE_TTL_SECS {
            return None;
        }

        let path = self.file_path(&entry.file_name);
        if !path.exists() {
            return None;
        }

        // If the query was not a direct URL, verify that the cached song is not a stale
        // music video with extra footage / behind-the-scenes when user didn't request that
        let is_direct_url = query.starts_with("http://") || query.starts_with("https://");
        if !is_direct_url {
            let q_lower = query.to_lowercase();
            let wants_bts = q_lower.contains("behind the scene")
                || q_lower.contains("bts")
                || q_lower.contains("making of")
                || q_lower.contains("extra footage")
                || q_lower.contains("blooper");
            let wants_video = q_lower.contains("video")
                || q_lower.contains("mv")
                || q_lower.contains("music video");

            let title_lower = entry.title.to_lowercase();
            let is_stale_bts = !wants_bts
                && (title_lower.contains("behind the scene")
                    || title_lower.contains("making of")
                    || title_lower.contains("extra footage")
                    || title_lower.contains("blooper"));
            let is_stale_mv = !wants_video
                && (title_lower.contains("official music video")
                    || title_lower.contains("official video"));

            if is_stale_bts || is_stale_mv {
                return None;
            }
        }

        entry.last_accessed = now;
        let song = SongInfo {
            id: entry.id.clone(),
            url: entry.url.clone(),
            title: entry.title.clone(),
        };
        self.save_index_locked(&index).await;
        Some((song, path))
    }

    async fn get_cached_path(&self, id: &str) -> Option<PathBuf> {
        let mut index = self.index.lock().await;
        let now = now_unix_secs();
        let entry = index.entries.get_mut(id)?;

        if now.saturating_sub(entry.created_at) > CACHE_TTL_SECS {
            return None;
        }

        let path = self.file_path(&entry.file_name);
        if !path.exists() {
            return None;
        }

        entry.last_accessed = now;
        self.save_index_locked(&index).await;
        Some(path)
    }

    async fn remember_query(&self, query: &str, id: &str) {
        let key = Self::normalize_query(query);
        let mut index = self.index.lock().await;
        index.queries.insert(key, id.to_string());
        self.save_index_locked(&index).await;
    }

    async fn insert_cached_file(&self, song: &SongInfo, query: Option<&str>, file_name: String) {
        let path = self.file_path(&file_name);
        let Ok(meta) = fs::metadata(&path) else {
            return;
        };
        if meta.len() == 0 {
            let _ = fs::remove_file(&path);
            return;
        }

        let now = now_unix_secs();
        {
            let mut index = self.index.lock().await;
            index.entries.insert(
                song.id.clone(),
                CacheEntry {
                    id: song.id.clone(),
                    title: song.title.clone(),
                    url: song.url.clone(),
                    file_name,
                    size_bytes: meta.len(),
                    created_at: now,
                    last_accessed: now,
                },
            );
            if let Some(q) = query {
                index.queries.insert(Self::normalize_query(q), song.id.clone());
            }
            self.save_index_locked(&index).await;
        }

        self.enforce_limits().await;
    }

    async fn stats(&self) -> (usize, u64) {
        let index = self.index.lock().await;
        let count = index.entries.len();
        let bytes = index.entries.values().map(|e| e.size_bytes).sum();
        (count, bytes)
    }
}

struct SharedTeeBuffer {
    buf: VecDeque<u8>,
    eof: bool,
    error: Option<String>,
    reader_dropped: bool,
}

struct TeeStreamReader {
    shared: Arc<(StdMutex<SharedTeeBuffer>, Condvar)>,
}

impl Read for TeeStreamReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap();
        loop {
            if !guard.buf.is_empty() {
                let n = out.len().min(guard.buf.len());
                for b in out.iter_mut().take(n) {
                    *b = guard.buf.pop_front().unwrap();
                }
                return Ok(n);
            }
            if let Some(ref err) = guard.error {
                return Err(std::io::Error::other(err.clone()));
            }
            if guard.eof {
                return Ok(0);
            }
            guard = cvar.wait(guard).unwrap();
        }
    }
}

impl Drop for TeeStreamReader {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        if let Ok(mut guard) = lock.lock() {
            guard.reader_dropped = true;
            guard.buf.clear();
            cvar.notify_all();
        }
    }
}

struct ActiveTrack {
    handle: TrackHandle,
    suppress_end: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default)]
enum StayMode {
    #[default]
    Default,
    Hours(f64),
    Forever,
}

struct BotState {
    queues: Mutex<HashMap<GuildId, VecDeque<SongInfo>>>,
    is_loop: Mutex<HashMap<GuildId, bool>>,
    stay_modes: Mutex<HashMap<GuildId, StayMode>>,
    current_songs: Mutex<HashMap<GuildId, SongInfo>>,
    current_tracks: Mutex<HashMap<GuildId, ActiveTrack>>,
    disconnect_timers: Mutex<HashMap<GuildId, AbortHandle>>,
    bot_user_id: Mutex<Option<UserId>>,
    http_client: reqwest::Client,
    spotify_regex: Regex,
    youtube_id_regex: Regex,
    cache: Arc<SongCache>,
    is_dev_instance: bool,
    dev_mode: AtomicBool,
}

impl BotState {
    fn new(is_dev_instance: bool) -> Self {
        let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "cache".to_string());
        Self {
            queues: Mutex::new(HashMap::new()),
            is_loop: Mutex::new(HashMap::new()),
            stay_modes: Mutex::new(HashMap::new()),
            current_songs: Mutex::new(HashMap::new()),
            current_tracks: Mutex::new(HashMap::new()),
            disconnect_timers: Mutex::new(HashMap::new()),
            bot_user_id: Mutex::new(None),
            http_client: reqwest::Client::new(),
            spotify_regex: Regex::new(r"https?://open\.spotify\.com/track/([a-zA-Z0-9]+)")
                .expect("Valid Spotify regex"),
            youtube_id_regex: Regex::new(r"(?:v=|youtu\.be/)([a-zA-Z0-9_-]{11})")
                .expect("Valid YouTube ID regex"),
            cache: Arc::new(SongCache::new(cache_dir)),
            is_dev_instance,
            // When you launch `cargo run` locally, dev_mode starts true on the local instance;
            // on the production container, dev_mode starts false until `!!dev on` is sent.
            dev_mode: AtomicBool::new(is_dev_instance),
        }
    }

    fn should_handle_commands(&self) -> bool {
        let dev_enabled = self.dev_mode.load(Ordering::SeqCst);
        if self.is_dev_instance {
            dev_enabled
        } else {
            !dev_enabled
        }
    }

    async fn cancel_disconnect_timer(&self, guild_id: GuildId) {
        if let Some(abort_handle) = self.disconnect_timers.lock().await.remove(&guild_id) {
            abort_handle.abort();
        }
    }

    async fn cleanup_guild(&self, guild_id: GuildId) {
        if let Some(active) = self.current_tracks.lock().await.remove(&guild_id) {
            active.suppress_end.store(true, Ordering::SeqCst);
            let _ = active.handle.stop();
        }
        self.queues.lock().await.remove(&guild_id);
        self.current_songs.lock().await.remove(&guild_id);
        self.is_loop.lock().await.remove(&guild_id);
        self.stay_modes.lock().await.remove(&guild_id);
        self.cancel_disconnect_timer(guild_id).await;
    }

    async fn is_playing(&self, guild_id: GuildId) -> bool {
        let handle = {
            let tracks = self.current_tracks.lock().await;
            tracks.get(&guild_id).map(|t| t.handle.clone())
        };
        if let Some(handle) = handle {
            if let Ok(info) = handle.get_info().await {
                return matches!(info.playing, PlayMode::Play);
            }
        }
        false
    }

    async fn is_paused(&self, guild_id: GuildId) -> bool {
        let handle = {
            let tracks = self.current_tracks.lock().await;
            tracks.get(&guild_id).map(|t| t.handle.clone())
        };
        if let Some(handle) = handle {
            if let Ok(info) = handle.get_info().await {
                return matches!(info.playing, PlayMode::Pause);
            }
        }
        false
    }

    async fn is_active(&self, guild_id: GuildId) -> bool {
        let handle = {
            let tracks = self.current_tracks.lock().await;
            tracks.get(&guild_id).map(|t| t.handle.clone())
        };
        if let Some(handle) = handle {
            if let Ok(info) = handle.get_info().await {
                return matches!(info.playing, PlayMode::Play | PlayMode::Pause);
            }
        }
        false
    }
}

#[derive(Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct SpotifyTrackResponse {
    name: String,
    artists: Vec<SpotifyArtist>,
}

#[derive(Deserialize)]
struct SpotifyArtist {
    name: String,
}

async fn fetch_spotify_track(
    http_client: &reqwest::Client,
    track_id: &str,
) -> Result<(String, String)> {
    let client_id = env::var("SPOTIPY_CLIENT_ID")
        .or_else(|_| env::var("SPOTIFY_CLIENT_ID"))
        .context("SPOTIPY_CLIENT_ID not set in environment")?;
    let client_secret = env::var("SPOTIPY_CLIENT_SECRET")
        .or_else(|_| env::var("SPOTIFY_CLIENT_SECRET"))
        .context("SPOTIPY_CLIENT_SECRET not set in environment")?;

    let auth = BASE64.encode(format!("{client_id}:{client_secret}"));

    let token_res: SpotifyTokenResponse = http_client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {auth}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("grant_type=client_credentials")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let track_res: SpotifyTrackResponse = http_client
        .get(format!("https://api.spotify.com/v1/tracks/{track_id}"))
        .header("Authorization", format!("Bearer {}", token_res.access_token))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let artist = track_res
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();

    Ok((track_res.name, artist))
}

fn ytdl_program() -> &'static str {
    if Path::new("yt-dlp.exe").exists() {
        "./yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

fn spawn_ytdl_child(url: &str) -> Result<Child> {
    let mut cmd = StdCommand::new(ytdl_program());
    cmd.args([
        "-o",
        "-",
        "-q",
        "--no-warnings",
        "--no-playlist",
        "--no-check-certificate",
        "-f",
        "ba[abr>0][vcodec=none]/best",
    ]);
    if Path::new("cookies.txt").exists() {
        cmd.args(["--cookies", "cookies.txt"]);
    }
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    cmd.spawn()
        .map_err(|e| anyhow!("Failed to spawn yt-dlp: {e}"))
}

/// Streams audio from `yt-dlp` into Songbird immediately while simultaneously
/// saving the complete audio stream into the disk cache (`cache/<id>.audio`).
fn spawn_tee_cached_stream(song: SongInfo, cache: Arc<SongCache>) -> Result<Input> {
    let mut child = spawn_ytdl_child(&song.url)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture yt-dlp stdout"))?;

    let safe_id = SongCache::sanitize_id(&song.id);
    let final_file_name = format!("{safe_id}.audio");
    let part_file_name = format!("{safe_id}.{}.part", now_unix_secs());
    let part_path = cache.file_path(&part_file_name);
    let final_path = cache.file_path(&final_file_name);

    let shared = Arc::new((
        StdMutex::new(SharedTeeBuffer {
            buf: VecDeque::with_capacity(64 * 1024),
            eof: false,
            error: None,
            reader_dropped: false,
        }),
        Condvar::new(),
    ));

    let shared_writer = shared.clone();
    let rt_handle = tokio::runtime::Handle::current();

    std::thread::spawn(move || {
        let mut part_file = File::create(&part_path).ok();
        let mut chunk = [0u8; 32 * 1024];
        let mut total_written: u64 = 0;
        let mut download_complete = false;

        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    download_complete = true;
                    break;
                }
                Ok(n) => {
                    if let Some(ref mut f) = part_file {
                        if total_written + (n as u64) <= MAX_CACHE_BYTES {
                            if f.write_all(&chunk[..n]).is_err() {
                                part_file = None;
                            } else {
                                total_written += n as u64;
                            }
                        } else {
                            part_file = None;
                        }
                    }

                    let (lock, cvar) = &*shared_writer;
                    let mut guard = lock.lock().unwrap();
                    if !guard.reader_dropped {
                        guard.buf.extend(&chunk[..n]);
                        cvar.notify_all();
                    }
                }
                Err(e) => {
                    let (lock, cvar) = &*shared_writer;
                    let mut guard = lock.lock().unwrap();
                    guard.error = Some(e.to_string());
                    cvar.notify_all();
                    break;
                }
            }
        }

        let status_ok = child.wait().map(|s| s.success()).unwrap_or(false);
        {
            let (lock, cvar) = &*shared_writer;
            let mut guard = lock.lock().unwrap();
            guard.eof = true;
            cvar.notify_all();
        }

        drop(part_file);

        if download_complete && status_ok && total_written > 0 {
            if fs::rename(&part_path, &final_path).is_ok() {
                rt_handle.spawn(async move {
                    cache.insert_cached_file(&song, None, final_file_name).await;
                });
                return;
            }
        }

        let _ = fs::remove_file(&part_path);
    });

    let reader = TeeStreamReader { shared };
    let audio_stream = AudioStream {
        input: Box::new(ReadOnlySource::new(reader)) as Box<dyn MediaSource>,
    };
    Ok(Input::Live(LiveInput::Raw(audio_stream), None))
}

/// Pre-downloads a queued song in the background so it is ready on disk before its turn.
fn prefetch_song_in_background(song: SongInfo, cache: Arc<SongCache>) {
    tokio::spawn(async move {
        if cache.get_cached_path(&song.id).await.is_some() {
            return;
        }

        let safe_id = SongCache::sanitize_id(&song.id);
        let final_file_name = format!("{safe_id}.audio");
        let part_file_name = format!("{safe_id}.prefetch.{}.part", now_unix_secs());
        let part_path = cache.file_path(&part_file_name);
        let final_path = cache.file_path(&final_file_name);

        let mut cmd = tokio::process::Command::new(ytdl_program());
        cmd.args([
            "-q",
            "--no-warnings",
            "--no-playlist",
            "--no-check-certificate",
            "-f",
            "ba[abr>0][vcodec=none]/best",
            "-o",
        ]);
        cmd.arg(&part_path);
        if Path::new("cookies.txt").exists() {
            cmd.args(["--cookies", "cookies.txt"]);
        }
        cmd.arg(&song.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Ok(status) = cmd.status().await {
            if status.success() && part_path.exists() {
                if fs::rename(&part_path, &final_path).is_ok() {
                    cache.insert_cached_file(&song, None, final_file_name).await;
                    return;
                }
            }
        }
        let _ = fs::remove_file(&part_path);
    });
}

#[derive(Clone, Debug)]
struct SearchCandidate {
    id: String,
    title: String,
    channel: String,
    duration: Option<u64>,
    url: String,
}

async fn fetch_search_candidates(target: &str) -> Result<Vec<SearchCandidate>> {
    let mut cmd = tokio::process::Command::new(ytdl_program());
    cmd.args([
        "--flat-playlist",
        "--print",
        "%(id)s\t%(title)s\t%(channel,uploader)s\t%(duration)s\t%(webpage_url,url)s",
        "--no-playlist",
        "--no-warnings",
        "--no-check-certificate",
    ]);
    if Path::new("cookies.txt").exists() {
        cmd.args(["--cookies", "cookies.txt"]);
    }
    cmd.arg(target);

    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("Could not run yt-dlp: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut candidates = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let id = parts[0].trim().to_string();
        if id.is_empty() || id == "NA" {
            continue;
        }
        let title = parts[1].trim().to_string();
        if title.is_empty() || title == "NA" {
            continue;
        }
        let channel = parts.get(2).map(|s| s.trim()).unwrap_or("").to_string();
        let duration = parts.get(3).and_then(|s| s.trim().parse::<u64>().ok());
        let url = parts
            .get(4)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != "NA")
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));

        candidates.push(SearchCandidate {
            id,
            title,
            channel,
            duration,
            url,
        });
    }

    Ok(candidates)
}

fn score_candidate(
    candidate: &SearchCandidate,
    rank: usize,
    query: &str,
    wants_bts: bool,
    wants_video: bool,
) -> i32 {
    let title_lower = candidate.title.to_lowercase();
    let channel_lower = candidate.channel.to_lowercase();
    let q_lower = query.to_lowercase();

    let mut score: i32 = 0;

    // 1. YouTube ranking preference (higher in search results gets a small head start)
    score += (10 - (rank.min(10) as i32)) * 2;

    // 2. Reject behind the scenes, bloopers, making-of, extra footage, interviews, reactions
    if !wants_bts {
        let bts_markers = [
            "behind the scene",
            "behind the scenes",
            "making of",
            "extra footage",
            "deleted scene",
            "blooper",
            "bloopers",
            "outtake",
            "outtakes",
            "b-roll",
            "interview",
            "reaction",
            "reacts",
            "review",
            "short film",
            "teaser",
            "trailer",
        ];
        if bts_markers.iter().any(|&m| title_lower.contains(m))
            || title_lower.contains("(bts)")
            || title_lower.contains("[bts]")
            || title_lower.contains(" bts ")
            || title_lower.starts_with("bts ")
            || title_lower.ends_with(" bts")
        {
            return -10000;
        }
    }

    // 3. Official music video penalty when user just wants the audio track
    // (Music videos frequently contain talking, dialogue, intro skits, movie interludes, gunfights)
    if !wants_video {
        if title_lower.contains("official music video")
            || title_lower.contains("official video")
            || title_lower.contains("music video")
            || title_lower.contains("(mv)")
            || title_lower.contains("[mv]")
        {
            score -= 40;
        }
    }

    // 4. Boost audio-only and clean music releases
    if channel_lower.ends_with("- topic") || channel_lower.contains("- topic") {
        // YouTube's official label track (Topic channel)
        score += 60;
    }
    if title_lower.contains("official audio") {
        score += 50;
    } else if title_lower.contains("audio") || title_lower.contains("(audio)") || title_lower.contains("[audio]") {
        score += 35;
    }

    if title_lower.contains("official lyric video") || title_lower.contains("lyric video") || title_lower.contains("lyrics") {
        score += 30;
    }

    if title_lower.contains("visualizer") || title_lower.contains("official visualizer") {
        score += 25;
    }

    if title_lower.contains("album version") || title_lower.contains("original mix") || title_lower.contains("studio version") || title_lower.contains("clean") {
        score += 15;
    }

    // 5. Duration sanity checks
    if let Some(dur) = candidate.duration {
        // Less than 30s is likely a snippet, teaser, or ringtone
        if dur < 30 {
            score -= 100;
        }

        let is_long_format_query = q_lower.contains("hour")
            || q_lower.contains("mix")
            || q_lower.contains("compilation")
            || q_lower.contains("podcast")
            || q_lower.contains("full album")
            || q_lower.contains("lofi")
            || q_lower.contains("lo-fi")
            || q_lower.contains("chill")
            || q_lower.contains("relax")
            || q_lower.contains("study")
            || q_lower.contains("bgm")
            || q_lower.contains("stream");

        if !is_long_format_query {
            if dur > 900 {
                score -= 500;
            } else if dur > 600 {
                score -= 100;
            }
        }
    }

    // 6. Query keyword match relevance
    let query_words: Vec<&str> = q_lower
        .split_whitespace()
        .filter(|w| w.len() > 1 && *w != "audio" && *w != "the" && *w != "and" && *w != "song")
        .collect();

    for word in &query_words {
        if title_lower.contains(word) || channel_lower.contains(word) {
            score += 15;
        }
    }

    score
}

async fn extract_song_info(state: &BotState, query: &str) -> Result<SongInfo> {
    // 1. Check if this exact query or URL is already in the 7-day cache
    if let Some((cached_song, _path)) = state.cache.get_by_query(query).await {
        return Ok(cached_song);
    }

    // Also check if query is a direct YouTube URL whose 11-char video ID is already cached
    if let Some(caps) = state.youtube_id_regex.captures(query) {
        if let Some(id_match) = caps.get(1) {
            let video_id = id_match.as_str();
            if state.cache.get_cached_path(video_id).await.is_some() {
                let index = state.cache.index.lock().await;
                if let Some(entry) = index.entries.get(video_id) {
                    return Ok(SongInfo {
                        id: entry.id.clone(),
                        url: entry.url.clone(),
                        title: entry.title.clone(),
                    });
                }
            }
        }
    }

    let is_direct_url = query.starts_with("http://") || query.starts_with("https://");

    if is_direct_url {
        let candidates = fetch_search_candidates(query).await?;
        let first = candidates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No results found for URL."))?;

        let final_url = if first.url.starts_with("http://") || first.url.starts_with("https://") {
            first.url
        } else {
            format!("https://www.youtube.com/watch?v={}", first.id)
        };

        let song = SongInfo {
            id: first.id,
            url: final_url,
            title: first.title,
        };

        state.cache.remember_query(query, &song.id).await;
        return Ok(song);
    }

    let q_lower = query.to_lowercase();
    let wants_bts = q_lower.contains("behind the scene")
        || q_lower.contains("bts")
        || q_lower.contains("making of")
        || q_lower.contains("extra footage")
        || q_lower.contains("blooper")
        || q_lower.contains("deleted scene")
        || q_lower.contains("interview");

    let wants_video = q_lower.contains("video")
        || q_lower.contains("mv")
        || q_lower.contains("music video")
        || q_lower.contains("short film");

    let already_audio_focused = q_lower.contains("audio")
        || q_lower.contains("lyrics")
        || q_lower.contains("lyric")
        || q_lower.contains("topic");

    // First attempt: fine-tuned audio search if user did not request BTS or video explicitly
    let target = if wants_bts || wants_video || already_audio_focused {
        format!("ytsearch10:{query}")
    } else {
        format!("ytsearch10:{query} audio")
    };

    let candidates = fetch_search_candidates(&target).await.unwrap_or_default();

    let mut scored: Vec<(i32, SearchCandidate)> = candidates
        .into_iter()
        .enumerate()
        .map(|(rank, c)| {
            let score = score_candidate(&c, rank, query, wants_bts, wants_video);
            (score, c)
        })
        .filter(|(score, _)| *score > -5000)
        .collect();

    // If searching with "audio" returned no valid candidates, fall back to plain query
    if scored.is_empty() && !wants_bts && !wants_video && !already_audio_focused {
        let fallback_target = format!("ytsearch10:{query}");
        if let Ok(fallback_candidates) = fetch_search_candidates(&fallback_target).await {
            scored = fallback_candidates
                .into_iter()
                .enumerate()
                .map(|(rank, c)| {
                    let score = score_candidate(&c, rank, query, wants_bts, wants_video);
                    (score, c)
                })
                .filter(|(score, _)| *score > -5000)
                .collect();
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0));

    let best = scored
        .into_iter()
        .next()
        .map(|(_, c)| c)
        .ok_or_else(|| anyhow!("No results found."))?;

    let final_title = if !best.channel.is_empty()
        && best.channel != "NA"
        && (best.channel.to_lowercase().ends_with("- topic") || best.channel.to_lowercase().contains("- topic"))
    {
        let clean_artist = best
            .channel
            .replace("- Topic", "")
            .replace("- topic", "")
            .trim()
            .to_string();
        if !clean_artist.is_empty() && !best.title.to_lowercase().contains(&clean_artist.to_lowercase()) {
            format!("{clean_artist} - {}", best.title)
        } else {
            best.title
        }
    } else {
        best.title
    };

    let final_url = format!("https://www.youtube.com/watch?v={}", best.id);

    state.cache.remember_query(query, &best.id).await;

    Ok(SongInfo {
        id: best.id,
        url: final_url,
        title: final_title,
    })
}

struct TrackEndNotifier {
    ctx: Context,
    state: Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
    suppress_end: Arc<AtomicBool>,
}

#[async_trait]
impl VoiceEventHandler for TrackEndNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(tracks) = ctx {
            for (state, _handle) in *tracks {
                if let PlayMode::Errored(ref err) = state.playing {
                    eprintln!("Track playback error: {err:?}");
                }
            }
        }
        if self.suppress_end.load(Ordering::SeqCst) {
            return None;
        }
        let ctx = self.ctx.clone();
        let state = self.state.clone();
        let guild_id = self.guild_id;
        let channel_id = self.channel_id;

        tokio::spawn(async move {
            play_next_song(&ctx, &state, guild_id, channel_id).await;
        });

        None
    }
}

fn start_disconnect_timer(
    ctx: Context,
    state: Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
) {
    tokio::spawn(async move {
        state.cancel_disconnect_timer(guild_id).await;

        let stay_mode = state
            .stay_modes
            .lock()
            .await
            .get(&guild_id)
            .copied()
            .unwrap_or_default();

        let sleep_duration = match stay_mode {
            StayMode::Forever => return,
            StayMode::Hours(hours) => Duration::from_secs_f64((hours * 3600.0).max(1.0)),
            StayMode::Default => Duration::from_secs(120),
        };

        let state_clone = state.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(sleep_duration).await;

            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_some() && !state_clone.is_playing(guild_id).await {
                let _ = manager.remove(guild_id).await;
                state_clone.cleanup_guild(guild_id).await;
                let _ = channel_id
                    .say(
                        &ctx.http,
                        "Auto-disconnected from voice channel due to inactivity.",
                    )
                    .await;
            }
        });

        state
            .disconnect_timers
            .lock()
            .await
            .insert(guild_id, task.abort_handle());
    });
}

async fn play_song_on_call(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
    song: &SongInfo,
) -> Result<()> {
    let manager = songbird::get(ctx)
        .await
        .ok_or_else(|| anyhow!("Songbird voice manager not initialized"))?;

    let handler_lock = manager
        .get(guild_id)
        .ok_or_else(|| anyhow!("Not connected to voice channel"))?;

    state.cancel_disconnect_timer(guild_id).await;

    if let Some(prev) = state.current_tracks.lock().await.remove(&guild_id) {
        prev.suppress_end.store(true, Ordering::SeqCst);
        let _ = prev.handle.stop();
    }

    // Play directly from disk cache if available (0ms wait on loop or replay!),
    // otherwise stream live from yt-dlp while teeing into the disk cache.
    let raw_input = if let Some(cached_path) = state.cache.get_cached_path(&song.id).await {
        match tokio::fs::read(&cached_path).await {
            Ok(bytes) if !bytes.is_empty() => Input::from(bytes),
            _ => spawn_tee_cached_stream(song.clone(), state.cache.clone())?,
        }
    } else {
        spawn_tee_cached_stream(song.clone(), state.cache.clone())?
    };

    let input = raw_input
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|e| anyhow!("Failed to prepare audio stream: {e}"))?;

    let track_handle = {
        let mut handler = handler_lock.lock().await;
        handler.play_input(input)
    };

    let suppress_end = Arc::new(AtomicBool::new(false));
    let _ = track_handle.add_event(
        Event::Track(TrackEvent::End),
        TrackEndNotifier {
            ctx: ctx.clone(),
            state: state.clone(),
            guild_id,
            channel_id,
            suppress_end: suppress_end.clone(),
        },
    );

    state.current_tracks.lock().await.insert(
        guild_id,
        ActiveTrack {
            handle: track_handle,
            suppress_end,
        },
    );

    Ok(())
}

async fn play_next_song(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
) {
    let manager = match songbird::get(ctx).await {
        Some(m) => m,
        None => return,
    };

    if manager.get(guild_id).is_none() {
        return;
    }

    let next_queued = {
        let mut queues = state.queues.lock().await;
        queues.get_mut(&guild_id).and_then(|q| q.pop_front())
    };

    if let Some(next_song) = next_queued {
        state
            .current_songs
            .lock()
            .await
            .insert(guild_id, next_song.clone());

        if let Err(err) = play_song_on_call(ctx, state, guild_id, channel_id, &next_song).await {
            eprintln!("Error playing next song: {err:?}");
            let _ = channel_id.say(&ctx.http, format!("Error: {err}")).await;
            return;
        }

        let _ = channel_id
            .say(&ctx.http, format!("Now playing: `{}`", next_song.title))
            .await;
    } else {
        let should_loop = state
            .is_loop
            .lock()
            .await
            .get(&guild_id)
            .copied()
            .unwrap_or(false);
        let current_song = state.current_songs.lock().await.get(&guild_id).cloned();

        if should_loop {
            if let Some(song) = current_song {
                if let Err(err) = play_song_on_call(ctx, state, guild_id, channel_id, &song).await {
                    eprintln!("Error playing looped song: {err:?}");
                    let _ = channel_id.say(&ctx.http, format!("Error: {err}")).await;
                    return;
                }

                let _ = channel_id
                    .say(
                        &ctx.http,
                        format!("Now playing in looping: `{}`", song.title),
                    )
                    .await;
                return;
            }
        }

        state.current_tracks.lock().await.remove(&guild_id);
        let _ = channel_id
            .say(&ctx.http, "No more songs in the queue.")
            .await;
        start_disconnect_timer(ctx.clone(), state.clone(), guild_id, channel_id);
    }
}

struct Handler {
    state: Arc<BotState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        *self.state.bot_user_id.lock().await = Some(ready.user.id);
        let mode_label = if self.state.is_dev_instance {
            "LOCAL DEV"
        } else {
            "PRODUCTION"
        };
        println!("{} is now jamming. [{}]", ready.user.name, mode_label);
        println!("Connected to {} guilds.", ready.guilds.len());
        for guild in &ready.guilds {
            let name = match ctx.cache.guild(guild.id).map(|g| g.name.clone()) {
                Some(name) => name,
                None => guild
                    .id
                    .to_partial_guild(&ctx.http)
                    .await
                    .map(|g| g.name)
                    .unwrap_or_else(|_| "Unknown".to_string()),
            };
            println!(" - {} (ID: {})", name, guild.id);
        }
        if self.state.is_dev_instance {
            println!(
                "Tip: Send `!!dev on` in Discord to pause the production DewaCloud bot while testing locally, and `!!dev off` when finished."
            );
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let bot_id = *self.state.bot_user_id.lock().await;
        if Some(new.user_id) == bot_id && new.channel_id.is_none() {
            let guild_id = new.guild_id.or_else(|| old.and_then(|o| o.guild_id));
            if let Some(guild_id) = guild_id {
                if let Some(manager) = songbird::get(&ctx).await {
                    let _ = manager.remove(guild_id).await;
                }
                self.state.cleanup_guild(guild_id).await;
            }
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let Some(guild_id) = msg.guild_id else {
            return;
        };

        let content = msg.content.as_str();

        // Handle `!!dev` (or `!!dev on` / `!!dev off`) across both prod & local instances
        if content == "!!dev" || content.starts_with("!!dev ") {
            let arg = content.strip_prefix("!!dev").unwrap_or("").trim();
            let enable_dev = match arg {
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                _ => !self.state.dev_mode.load(Ordering::SeqCst),
            };

            self.state.dev_mode.store(enable_dev, Ordering::SeqCst);

            // If this instance is entering standby, disconnect from voice in this guild
            if !self.state.should_handle_commands() {
                if let Some(manager) = songbird::get(&ctx).await {
                    if manager.get(guild_id).is_some() {
                        self.state.cleanup_guild(guild_id).await;
                        let _ = manager.remove(guild_id).await;
                    }
                }
            }

            // Only the production instance (or the local instance if prod isn't running) sends confirmation
            // Prefer sending from production instance so only 1 message is ever sent when both are online:
            if !self.state.is_dev_instance {
                let status_msg = if enable_dev {
                    "🛠️ **Dev mode enabled** — Production bot is now on standby. Local dev bot will handle commands."
                } else {
                    "✅ **Dev mode disabled** — Production bot is active again."
                };
                let _ = msg.channel_id.say(&ctx.http, status_msg).await;
            }
            return;
        }

        // Ignore all other commands if this instance is on standby
        if !self.state.should_handle_commands() {
            return;
        }

        if content.starts_with("!!play") {
            let user_channel_id = ctx.cache.guild(guild_id).and_then(|guild| {
                guild
                    .voice_states
                    .get(&msg.author.id)
                    .and_then(|vs| vs.channel_id)
            });

            let Some(connect_to) = user_channel_id else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "You need to be in a voice channel!")
                    .await;
                return;
            };

            {
                let mut queues = self.state.queues.lock().await;
                queues.entry(guild_id).or_insert_with(VecDeque::new);
            }
            {
                let mut is_loop = self.state.is_loop.lock().await;
                is_loop.entry(guild_id).or_insert(false);
            }

            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_none() {
                if let Err(err) = manager.join(guild_id, connect_to).await {
                    eprintln!("Error joining voice channel: {err:?}");
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, format!("Error: {err}"))
                        .await;
                    return;
                }
            }

            let mut query = content
                .strip_prefix("!!play")
                .unwrap_or("")
                .trim()
                .to_string();

            if query.is_empty() {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Searching for ``...")
                    .await;
                let _ = msg.channel_id.say(&ctx.http, "No results found.").await;
                return;
            }

            if query.contains("open.spotify.com/track") {
                if let Some(caps) = self.state.spotify_regex.captures(&query) {
                    if let Some(track_id_match) = caps.get(1) {
                        match fetch_spotify_track(&self.state.http_client, track_id_match.as_str())
                            .await
                        {
                            Ok((track_title, track_artist)) => {
                                query = format!("{track_title} {track_artist}");
                                let _ = msg
                                    .channel_id
                                    .say(
                                        &ctx.http,
                                        format!(
                                            "Found Spotify track: `{track_title}` by `{track_artist}`. Searching on YouTube..."
                                        ),
                                    )
                                    .await;
                            }
                            Err(err) => {
                                eprintln!("Error during Spotify lookup: {err:?}");
                                let _ = msg
                                    .channel_id
                                    .say(&ctx.http, format!("Error: {err}"))
                                    .await;
                                return;
                            }
                        }
                    }
                }
            } else if self.state.cache.get_by_query(&query).await.is_none() {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, format!("Searching for `{query}`..."))
                    .await;
            }

            match extract_song_info(&self.state, &query).await {
                Ok(song_info) => {
                    let title = song_info.title.clone();
                    let song_clone = song_info.clone();
                    let position = {
                        let mut queues = self.state.queues.lock().await;
                        let queue = queues.entry(guild_id).or_insert_with(VecDeque::new);
                        queue.push_back(song_info);
                        queue.len()
                    };

                    if !self.state.is_active(guild_id).await {
                        play_next_song(&ctx, &self.state, guild_id, msg.channel_id).await;
                    } else {
                        // Pre-download queued song into cache so it plays with 0 wait when its turn arrives
                        prefetch_song_in_background(song_clone, self.state.cache.clone());
                        let _ = msg
                            .channel_id
                            .say(
                                &ctx.http,
                                format!("Added to queue (#{position}): `{title}`"),
                            )
                            .await;
                    }
                }
                Err(err) => {
                    eprintln!("Error during playback setup: {err:?}");
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, format!("Error: {err}"))
                        .await;
                }
            }
        } else if content.starts_with("!!queue") {
            let queue_list = {
                let queues = self.state.queues.lock().await;
                queues.get(&guild_id).and_then(|q| {
                    if q.is_empty() {
                        None
                    } else {
                        Some(
                            q.iter()
                                .enumerate()
                                .map(|(i, song)| format!("{}. {}", i + 1, song.title))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                })
            };

            if let Some(queue_list) = queue_list {
                let current = self
                    .state
                    .current_songs
                    .lock()
                    .await
                    .get(&guild_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| "Nothing".to_string());

                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        format!("**Now Playing:** `{current}`\n\n**Queue:**\n{queue_list}"),
                    )
                    .await;
            } else {
                let _ = msg.channel_id.say(&ctx.http, "Queue is empty!").await;
            }
        } else if content.starts_with("!!skip") {
            if self.state.is_active(guild_id).await {
                let handle = {
                    let tracks = self.state.current_tracks.lock().await;
                    tracks.get(&guild_id).map(|t| t.handle.clone())
                };
                if let Some(handle) = handle {
                    let _ = handle.stop(); // Triggers TrackEndNotifier -> play_next_song
                }
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Skipped to next song.")
                    .await;
            } else {
                let _ = msg.channel_id.say(&ctx.http, "Nothing to skip.").await;
            }
        } else if content.starts_with("!!clear") {
            let mut queues = self.state.queues.lock().await;
            if let Some(queue) = queues.get_mut(&guild_id) {
                queue.clear();
                drop(queues);
                let _ = msg.channel_id.say(&ctx.http, "Queue cleared.").await;
            } else {
                drop(queues);
                let _ = msg.channel_id.say(&ctx.http, "No queue to clear.").await;
            }
        } else if content.starts_with("!!pause") {
            if self.state.is_playing(guild_id).await {
                let handle = {
                    let tracks = self.state.current_tracks.lock().await;
                    tracks.get(&guild_id).map(|t| t.handle.clone())
                };
                if let Some(handle) = handle {
                    if let Err(err) = handle.pause() {
                        eprintln!("Error during pause: {err:?}");
                    } else {
                        let _ = msg.channel_id.say(&ctx.http, "Music paused.").await;
                    }
                }
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "No music is playing.")
                    .await;
            }
        } else if content.starts_with("!!resume") {
            if self.state.is_paused(guild_id).await {
                let handle = {
                    let tracks = self.state.current_tracks.lock().await;
                    tracks.get(&guild_id).map(|t| t.handle.clone())
                };
                if let Some(handle) = handle {
                    if let Err(err) = handle.play() {
                        eprintln!("Error during resume: {err:?}");
                    } else {
                        let _ = msg.channel_id.say(&ctx.http, "Music resumed.").await;
                    }
                }
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "No music is paused.")
                    .await;
            }
        } else if content.starts_with("!!loop") {
            let enabled = {
                let mut is_loop = self.state.is_loop.lock().await;
                let current = is_loop.get(&guild_id).copied().unwrap_or(false);
                let next = !current;
                is_loop.insert(guild_id, next);
                next
            };
            let status = if enabled { "enabled" } else { "disabled" };
            let _ = msg
                .channel_id
                .say(&ctx.http, format!("Loop mode {status}"))
                .await;
        } else if content.starts_with("!!stop") {
            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_some() {
                if let Some(active) = self.state.current_tracks.lock().await.remove(&guild_id) {
                    active.suppress_end.store(true, Ordering::SeqCst);
                    let _ = active.handle.stop();
                }
                if let Some(queue) = self.state.queues.lock().await.get_mut(&guild_id) {
                    queue.clear();
                }
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Music stopped and queue cleared.")
                    .await;

                start_disconnect_timer(ctx.clone(), self.state.clone(), guild_id, msg.channel_id);
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "No music is playing.")
                    .await;
                self.state.cancel_disconnect_timer(guild_id).await;
            }
        } else if content == "!!stay" || content.starts_with("!!stay ") {
            let arg = content.strip_prefix("!!stay").unwrap_or("").trim().to_lowercase();

            if arg.is_empty() {
                let mode = self
                    .state
                    .stay_modes
                    .lock()
                    .await
                    .get(&guild_id)
                    .copied()
                    .unwrap_or_default();
                let current_str = match mode {
                    StayMode::Default => "default (2 minutes)".to_string(),
                    StayMode::Hours(h) => format!("{h} hour(s)"),
                    StayMode::Forever => "forever (until `!!disconnect`)".to_string(),
                };
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        format!(
                            "Current stay mode: `{current_str}`.\nUsage: `!!stay <hours>` (e.g. `!!stay 24`), `!!stay forever`, or `!!stay off`"
                        ),
                    )
                    .await;
                return;
            }

            // Ensure the bot is in a voice channel (or join the user's voice channel)
            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_none() {
                let user_channel_id = ctx.cache.guild(guild_id).and_then(|guild| {
                    guild
                        .voice_states
                        .get(&msg.author.id)
                        .and_then(|vs| vs.channel_id)
                });
                if let Some(connect_to) = user_channel_id {
                    if let Err(err) = manager.join(guild_id, connect_to).await {
                        let _ = msg
                            .channel_id
                            .say(&ctx.http, format!("Error joining voice channel: {err}"))
                            .await;
                        return;
                    }
                } else {
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "You need to be in a voice channel!")
                        .await;
                    return;
                }
            }

            if arg == "forever" {
                self.state
                    .stay_modes
                    .lock()
                    .await
                    .insert(guild_id, StayMode::Forever);
                self.state.cancel_disconnect_timer(guild_id).await;
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        "Stay mode set to `forever`. Will not disconnect until `!!disconnect` is called.",
                    )
                    .await;
            } else if arg == "off" || arg == "0" || arg == "default" {
                self.state
                    .stay_modes
                    .lock()
                    .await
                    .insert(guild_id, StayMode::Default);
                if !self.state.is_playing(guild_id).await {
                    start_disconnect_timer(ctx.clone(), self.state.clone(), guild_id, msg.channel_id);
                }
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Stay mode reset to default (2 minutes of inactivity).")
                    .await;
            } else if let Ok(hours) = arg.parse::<f64>() {
                if hours > 0.0 && hours.is_finite() {
                    self.state
                        .stay_modes
                        .lock()
                        .await
                        .insert(guild_id, StayMode::Hours(hours));
                    if !self.state.is_playing(guild_id).await {
                        start_disconnect_timer(
                            ctx.clone(),
                            self.state.clone(),
                            guild_id,
                            msg.channel_id,
                        );
                    }
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            format!(
                                "Stay mode set to `{hours} hour(s)`. Will stay in the voice channel for {hours} hour(s)."
                            ),
                        )
                        .await;
                } else {
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            "Invalid duration. Use a positive number of hours (e.g. `!!stay 24`) or `!!stay forever`.",
                        )
                        .await;
                }
            } else {
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        "Invalid argument. Usage: `!!stay <hours>` (e.g. `!!stay 24`), `!!stay forever`, or `!!stay off`.",
                    )
                    .await;
            }
        } else if content.starts_with("!!disconnect") {
            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_some() {
                self.state.cleanup_guild(guild_id).await;
                let _ = manager.remove(guild_id).await;
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Disconnected from voice channel.")
                    .await;
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Not connected to any voice channel.")
                    .await;
            }
        } else if content.starts_with("!!cache") {
            self.state.cache.enforce_limits().await;
            let (count, bytes) = self.state.cache.stats().await;
            let mb = (bytes as f64) / (1024.0 * 1024.0);
            let _ = msg
                .channel_id
                .say(
                    &ctx.http,
                    format!("Cached songs: `{count}` (`{mb:.2} MB` / `5120.00 MB`, TTL: 7 days)"),
                )
                .await;
        } else if content.starts_with("!!help") {
            let help_text = "\
## **Offside BOT**
> *High-performance music streaming with smart audio search*

### **Playback**
• `!!play <song | URL>` — Play a song or add to queue (YouTube & Spotify)
• `!!pause` — Pause current playback
• `!!resume` — Resume playback
• `!!skip` — Skip to the next song
• `!!stop` — Stop playback and clear the queue
• `!!loop` — Toggle loop mode for the current track

### **Queue**
• `!!queue` — Show currently playing song and upcoming queue
• `!!clear` — Clear all songs waiting in the queue

### **Voice Channel**
• `!!stay <hours | forever | off>` — Set inactivity stay duration (e.g. `!!stay 24`, `!!stay forever`)
• `!!disconnect` — Leave the voice channel

### **System**
• `!!cache` — View audio disk cache usage
• `!!dev [on | off]` — Toggle local dev mode / production standby
• `!!help` — Show this command reference

-# Developed with ❤️ by @evangelion.xyz";

            if let Err(why) = msg.channel_id.say(&ctx.http, help_text).await {
                eprintln!("Error sending message: {why:?}");
            }
        }
    }
}

fn load_env_file(path: &Path) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut normalized = String::with_capacity(content.len());

    for line in content.lines() {
        normalized.push_str(line);
        normalized.push('\n');
    }

    dotenvy::from_read(Cursor::new(normalized)).map_err(|error| error.to_string())
}

fn load_discord_token() -> String {
    if let Ok(token) = env::var("DISCORD_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Ok(token) = fs::read_to_string("token.txt") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    panic!("Expected DISCORD_TOKEN in environment/.env or a non-empty token.txt file!");
}

fn detect_dev_instance() -> bool {
    if let Ok(val) = env::var("BOT_ENV") {
        return !matches!(
            val.trim().to_lowercase().as_str(),
            "prod" | "production" | "release"
        );
    }
    cfg!(debug_assertions)
}

#[tokio::main]
async fn main() {
    let path = Path::new(".env");

    if path.exists() {
        if let Err(error) = load_env_file(path) {
            eprintln!(
                "Failed to load environment file {}: {:?}",
                path.display(),
                error
            );
        }
    }

    let token = load_discord_token();
    let is_dev_instance = detect_dev_instance();

    let state = Arc::new(BotState::new(is_dev_instance));
    state.cache.enforce_limits().await;

    // Periodic background task to prune expired (>7 days) or >5GB cache entries every hour
    {
        let cache = state.cache.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                cache.enforce_limits().await;
            }
        });
    }

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES;

    let handler = Handler { state };

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird()
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        eprintln!("Client error: {why:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_audio_search_thriller() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Michael Jackson Thriller").await.unwrap();
        println!("Resolved Thriller: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the 14-minute official 4K music video (id: sOnqjkJTMaA)
        assert_ne!(res.id, "sOnqjkJTMaA", "Must not pick the music video with movie intro/extra footage");
    }

    #[tokio::test]
    async fn test_audio_search_paparazzi() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Lady Gaga Paparazzi").await.unwrap();
        println!("Resolved Paparazzi: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the 7-minute music video with skits (d2smz_1L2_0 / qStFzmQGQNw)
        assert_ne!(res.id, "d2smz_1L2_0");
        assert_ne!(res.id, "qStFzmQGQNw");
    }

    #[tokio::test]
    async fn test_audio_search_payphone() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Maroon 5 Payphone").await.unwrap();
        println!("Resolved Payphone: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the music video with bank robbery gunfight (id: KRaWnd3LJfs)
        assert_ne!(res.id, "KRaWnd3LJfs");
    }

    #[tokio::test]
    async fn test_direct_url_preserved() {
        let state = BotState::new(true);
        let url = "https://www.youtube.com/watch?v=sOnqjkJTMaA";
        let res = extract_song_info(&state, url).await.unwrap();
        assert_eq!(res.id, "sOnqjkJTMaA");
    }

    #[tokio::test]
    async fn test_explicit_bts_request() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Michael Jackson Thriller making of behind the scenes").await.unwrap();
        println!("Resolved explicit BTS: id={}, title={}", res.id, res.title);
        let lower = res.title.to_lowercase();
        assert!(lower.contains("making") || lower.contains("behind") || lower.contains("thriller"));
    }
}
