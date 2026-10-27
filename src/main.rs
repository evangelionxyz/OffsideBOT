use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    io::Cursor,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use regex::Regex;
use serde::Deserialize;
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
        ChildContainer, Input,
        codecs::{get_codec_registry, get_probe},
    },
    tracks::{PlayMode, TrackHandle},
};
use tokio::{sync::Mutex, task::AbortHandle};

#[derive(Clone, Debug)]
struct SongInfo {
    url: String,
    title: String,
}

struct ActiveTrack {
    handle: TrackHandle,
    suppress_end: Arc<AtomicBool>,
}

struct BotState {
    queues: Mutex<HashMap<GuildId, VecDeque<SongInfo>>>,
    is_loop: Mutex<HashMap<GuildId, bool>>,
    current_songs: Mutex<HashMap<GuildId, SongInfo>>,
    current_tracks: Mutex<HashMap<GuildId, ActiveTrack>>,
    disconnect_timers: Mutex<HashMap<GuildId, AbortHandle>>,
    bot_user_id: Mutex<Option<UserId>>,
    http_client: reqwest::Client,
    spotify_regex: Regex,
}

impl BotState {
    fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            is_loop: Mutex::new(HashMap::new()),
            current_songs: Mutex::new(HashMap::new()),
            current_tracks: Mutex::new(HashMap::new()),
            disconnect_timers: Mutex::new(HashMap::new()),
            bot_user_id: Mutex::new(None),
            http_client: reqwest::Client::new(),
            spotify_regex: Regex::new(r"https?://open\.spotify\.com/track/([a-zA-Z0-9]+)")
                .expect("Valid Spotify regex"),
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

fn spawn_ytdl_stream(url: &str) -> Result<Input> {
    let mut cmd = std::process::Command::new(ytdl_program());
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn yt-dlp: {e}"))?;

    Ok(Input::from(ChildContainer::from(child)))
}

async fn extract_song_info(_http_client: &reqwest::Client, query: &str) -> Result<SongInfo> {
    let target = if query.starts_with("http://") || query.starts_with("https://") {
        query.to_string()
    } else {
        format!("ytsearch1:{query}")
    };

    let mut cmd = tokio::process::Command::new(ytdl_program());
    cmd.args([
        "--flat-playlist",
        "--print",
        "%(title)s",
        "--print",
        "%(webpage_url,url)s",
        "--no-playlist",
        "--no-warnings",
        "--no-check-certificate",
    ]);
    if Path::new("cookies.txt").exists() {
        cmd.args(["--cookies", "cookies.txt"]);
    }
    cmd.arg(&target);

    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("Could not run yt-dlp: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().map(str::trim).filter(|l| !l.is_empty());

    let title = lines
        .next()
        .ok_or_else(|| anyhow!("No results found."))?
        .to_string();
    let url = lines.next().unwrap_or(&target).to_string();

    Ok(SongInfo { url, title })
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

        let state_clone = state.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;

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

    let input = spawn_ytdl_stream(&song.url)?
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
                    .say(&ctx.http, format!("Now playing in looping: `{}`", song.title))
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
        println!("{} is now jamming.", ready.user.name);
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
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, format!("Searching for `{query}`..."))
                    .await;
            }

            match extract_song_info(&self.state.http_client, &query).await {
                Ok(song_info) => {
                    let title = song_info.title.clone();
                    let position = {
                        let mut queues = self.state.queues.lock().await;
                        let queue = queues.entry(guild_id).or_insert_with(VecDeque::new);
                        queue.push_back(song_info);
                        queue.len()
                    };

                    if !self.state.is_active(guild_id).await {
                        play_next_song(&ctx, &self.state, guild_id, msg.channel_id).await;
                    } else {
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
        } else if content.starts_with("!!help") {
            let help_text = concat!(
                "Offside BOT\n",
                "`Here are the commands you can use:\n",
                "- !!play <song name or URL> - Play a song\n",
                "- !!queue ------------------- Show the current queue\n",
                "- !!skip -------------------- Skip to the next song\n",
                "- !!clear ------------------- Clear the queue\n",
                "- !!pause ------------------- Pause the music\n",
                "- !!resume ------------------ Resume the music\n",
                "- !!loop -------------------- Toggle loop mode\n",
                "- !!stop -------------------- Stop the music and clear the queue\n",
                "- !!disconnect -------------- Disconnect from the voice channel\n",
                "\nDeveloped with ❤️ by @evangelion.xyz`"
            );

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

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES;

    let handler = Handler {
        state: Arc::new(BotState::new()),
    };

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird()
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        eprintln!("Client error: {why:?}");
    }
}
