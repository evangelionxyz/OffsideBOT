use std::{
    collections::VecDeque,
    sync::{atomic::Ordering, Arc},
};

use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{channel::Message, gateway::Ready, voice::VoiceState},
};

use crate::{
    models::StayMode,
    playback::{play_next_song, start_disconnect_timer},
    search::extract_song_info,
    spotify::fetch_spotify_track,
    state::BotState,
    stream::prefetch_song_in_background,
};

pub struct Handler {
    pub state: Arc<BotState>,
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
## 🎵 **Offside BOT**
> *High-performance music streaming with smart audio search*

### 🎶 **Playback**
• `!!play <song | URL>` — Play a song or add to queue (YouTube & Spotify)
• `!!pause` — Pause current playback
• `!!resume` — Resume playback
• `!!skip` — Skip to the next song
• `!!stop` — Stop playback and clear the queue
• `!!loop` — Toggle loop mode for the current track

### 📋 **Queue**
• `!!queue` — Show currently playing song and upcoming queue
• `!!clear` — Clear all songs waiting in the queue

### 🔊 **Voice Channel**
• `!!stay <hours | forever | off>` — Set inactivity stay duration (e.g. `!!stay 24`, `!!stay forever`)
• `!!disconnect` — Leave the voice channel

### ⚙️ **System**
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

