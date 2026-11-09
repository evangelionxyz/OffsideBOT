use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        channel::Message,
        gateway::Ready,
        id::{GuildId, UserId},
        voice::VoiceState,
    },
};

use crate::{
    models::StayMode,
    playback::{
        cancel_slot, clear_queue, fill_slot, remove_from_queue, reserve_slot, shuffle_queue,
        skip_current, start_disconnect_timer, stop_playback, FillResult,
    },
    resolvers::{fetch_apple_music_track, fetch_deezer_track, fetch_spotify_track},
    search::extract_song_info,
    state::BotState,
};

pub struct Handler {
    pub state: Arc<BotState>,
}

/// Counts non-bot members in a voice channel using the gateway cache.
fn humans_in_channel(
    ctx: &Context,
    guild_id: GuildId,
    channel: u64,
    bot_id: Option<UserId>,
) -> usize {
    ctx.cache
        .guild(guild_id)
        .map(|g| {
            g.voice_states
                .values()
                .filter(|vs| vs.channel_id.map(|c| c.get()) == Some(channel))
                .filter(|vs| Some(vs.user_id) != bot_id)
                .count()
        })
        .unwrap_or(1)
}

/// True if the user is in the same voice channel as the bot (or the bot isn't connected).
async fn in_bot_channel(ctx: &Context, guild_id: GuildId, user_id: UserId) -> bool {
    let user_channel = ctx
        .cache
        .guild(guild_id)
        .and_then(|g| g.voice_states.get(&user_id).and_then(|vs| vs.channel_id))
        .map(|c| c.get());

    let Some(manager) = songbird::get(ctx).await else {
        return true;
    };
    let Some(call) = manager.get(guild_id) else {
        return true;
    };
    let bot_channel = call.lock().await.current_channel().map(|c| c.0.get());
    bot_channel.is_none() || bot_channel == user_channel
}

/// Resolves Spotify / Apple Music / Deezer links to `(service, title, artist)`.
/// Returns `Ok(None)` for anything that isn't one of those links.
async fn resolve_external_link(
    state: &BotState,
    query: &str,
) -> anyhow::Result<Option<(&'static str, String, String)>> {
    if query.contains("open.spotify.com/track") {
        if let Some(id) = state
            .spotify_regex
            .captures(query)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
        {
            let (title, artist) = fetch_spotify_track(&state.http_client, id).await?;
            return Ok(Some(("Spotify", title, artist)));
        }
    } else if query.contains("music.apple.com") {
        if let Some(id) = state
            .apple_music_regex
            .captures(query)
            .and_then(|c| c.get(1).or_else(|| c.get(2)))
            .map(|m| m.as_str())
        {
            let (title, artist) = fetch_apple_music_track(&state.http_client, id).await?;
            return Ok(Some(("Apple Music", title, artist)));
        }
    } else if query.contains("deezer.com") {
        if let Some(id) = state
            .deezer_regex
            .captures(query)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
        {
            let (title, artist) = fetch_deezer_track(&state.http_client, id).await?;
            return Ok(Some(("Deezer", title, artist)));
        }
    }
    Ok(None)
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
            return;
        }

        // Leave when everyone else has left the bot's voice channel
        let Some(guild_id) = new.guild_id else {
            return;
        };
        if Some(new.user_id) == bot_id {
            return;
        }
        let Some(manager) = songbird::get(&ctx).await else {
            return;
        };
        let Some(call) = manager.get(guild_id) else {
            return;
        };
        let Some(bot_channel) = call.lock().await.current_channel().map(|c| c.0.get()) else {
            return;
        };
        if humans_in_channel(&ctx, guild_id, bot_channel, bot_id) > 0 {
            return;
        }

        let stay_mode = self
            .state
            .stay_modes
            .lock()
            .await
            .get(&guild_id)
            .copied()
            .unwrap_or_default();
        if matches!(stay_mode, StayMode::Forever) {
            return;
        }

        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let Some(manager) = songbird::get(&ctx).await else {
                return;
            };
            let Some(call) = manager.get(guild_id) else {
                return;
            };
            let current = call.lock().await.current_channel().map(|c| c.0.get());
            // Still in the same channel and still alone: leave
            if current == Some(bot_channel)
                && humans_in_channel(&ctx, guild_id, bot_channel, bot_id) == 0
            {
                state.cleanup_guild(guild_id).await;
                let _ = manager.remove(guild_id).await;
            }
        });
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

        let cmd = content.split_whitespace().next().unwrap_or("");

        // Playback controls only work for users sitting in the bot's voice channel
        const CHANNEL_GATED: &[&str] = &[
            "!!play", "!!skip", "!!stop", "!!pause", "!!resume", "!!loop", "!!clear", "!!remove",
            "!!shuffle", "!!volume", "!!stay", "!!disconnect",
        ];
        if CHANNEL_GATED.contains(&cmd) && !in_bot_channel(&ctx, guild_id, msg.author.id).await {
            let _ = msg
                .channel_id
                .say(&ctx.http, "You need to be in my voice channel to do that!")
                .await;
            return;
        }

        if cmd == "!!play" {
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

            let query = content
                .strip_prefix("!!play")
                .unwrap_or("")
                .trim()
                .to_string();

            if query.is_empty() {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Usage: `!!play <song | URL>`")
                    .await;
                return;
            }

            {
                let mut is_loop = self.state.is_loop.lock().await;
                is_loop.entry(guild_id).or_insert(false);
            }

            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_none() {
                match manager.join(guild_id, connect_to).await {
                    Ok(call) => {
                        let _ = call.lock().await.deafen(true).await;
                    }
                    Err(err) => {
                        eprintln!("Error joining voice channel: {err:?}");
                        let _ = msg
                            .channel_id
                            .say(&ctx.http, format!("Error: {err}"))
                            .await;
                        return;
                    }
                }
            }

            // Reserve the queue slot right away so order follows request order
            let (ticket, position) = match reserve_slot(&self.state, guild_id, &query).await {
                Ok(slot) => slot,
                Err(reason) => {
                    let _ = msg.channel_id.say(&ctx.http, reason).await;
                    return;
                }
            };

            let already_cached = self.state.cache.get_by_query(&query).await.is_some();
            let mut search_query = query.clone();

            let resolved = if already_cached {
                Ok(None)
            } else {
                resolve_external_link(&self.state, &query).await
            };

            match resolved {
                Ok(Some((service, track_title, track_artist))) => {
                    search_query = format!("{track_artist} - {track_title}");
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            format!(
                                "Found {service} track: `{track_title}` by `{track_artist}`. Searching on YouTube..."
                            ),
                        )
                        .await;
                }
                Ok(_) => {
                    if !already_cached {
                        let _ = msg
                            .channel_id
                            .say(
                                &ctx.http,
                                format!("Searching for `{query}`... (reserved #{position})"),
                            )
                            .await;
                    }
                }
                Err(err) => {
                    eprintln!("Error during link lookup: {err:?}");
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, format!("Error: {err}"))
                        .await;
                    cancel_slot(&ctx, &self.state, guild_id, msg.channel_id, ticket).await;
                    return;
                }
            }

            match extract_song_info(&self.state, &search_query).await {
                Ok(song_info) => {
                    if search_query != query {
                        self.state.cache.remember_query(&query, &song_info.id).await;
                    }
                    let title = song_info.title.clone();
                    match fill_slot(&ctx, &self.state, guild_id, msg.channel_id, ticket, song_info)
                        .await
                    {
                        FillResult::Queued(position) => {
                            let _ = msg
                                .channel_id
                                .say(
                                    &ctx.http,
                                    format!("Added to queue (#{position}): `{title}`"),
                                )
                                .await;
                        }
                        FillResult::Started | FillResult::Cancelled => {}
                    }
                }
                Err(err) => {
                    eprintln!("Error during playback setup: {err:?}");
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, format!("Error: {err}"))
                        .await;
                    cancel_slot(&ctx, &self.state, guild_id, msg.channel_id, ticket).await;
                }
            }
        } else if cmd == "!!queue" {
            let (entries, total) = {
                let queues = self.state.queues.lock().await;
                match queues.get(&guild_id) {
                    Some(q) => (q.iter().map(|e| e.label()).collect::<Vec<_>>(), q.len()),
                    None => (Vec::new(), 0),
                }
            };

            if total > 0 {
                let current = self
                    .state
                    .current_songs
                    .lock()
                    .await
                    .get(&guild_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| "Nothing".to_string());

                // Stay under Discord's 2000 character message limit
                let mut list = String::new();
                let mut shown = 0;
                for (i, label) in entries.iter().enumerate() {
                    let line = format!("{}. {}\n", i + 1, label);
                    if list.len() + line.len() > 1500 {
                        break;
                    }
                    list.push_str(&line);
                    shown += 1;
                }
                if shown < total {
                    list.push_str(&format!("...and {} more", total - shown));
                }

                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        format!("**Now Playing:** `{current}`\n\n**Queue ({total}):**\n{list}"),
                    )
                    .await;
            } else {
                let _ = msg.channel_id.say(&ctx.http, "Queue is empty!").await;
            }
        } else if cmd == "!!remove" {
            let arg = content.strip_prefix("!!remove").unwrap_or("").trim();
            match arg.parse::<usize>() {
                Ok(position) => match remove_from_queue(&self.state, guild_id, position).await {
                    Some(entry) => {
                        let _ = msg
                            .channel_id
                            .say(&ctx.http, format!("Removed `{}` from the queue.", entry.label()))
                            .await;
                    }
                    None => {
                        let _ = msg
                            .channel_id
                            .say(&ctx.http, "No song at that queue position.")
                            .await;
                    }
                },
                Err(_) => {
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "Usage: `!!remove <queue position>`")
                        .await;
                }
            }
        } else if cmd == "!!shuffle" {
            let count = shuffle_queue(&self.state, guild_id).await;
            let reply = if count > 1 {
                format!("Shuffled {count} songs.")
            } else {
                "Not enough songs in the queue to shuffle.".to_string()
            };
            let _ = msg.channel_id.say(&ctx.http, reply).await;
        } else if cmd == "!!nowplaying" || cmd == "!!np" {
            let current = self.state.current_songs.lock().await.get(&guild_id).cloned();
            let handle = {
                let tracks = self.state.current_tracks.lock().await;
                tracks.get(&guild_id).map(|t| t.handle.clone())
            };
            match (current, handle) {
                (Some(song), Some(handle)) => {
                    let elapsed = handle
                        .get_info()
                        .await
                        .map(|i| i.position.as_secs())
                        .unwrap_or(0);
                    let _ = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            format!(
                                "**Now Playing:** `{}` ({}:{:02})\n{}",
                                song.title,
                                elapsed / 60,
                                elapsed % 60,
                                song.url
                            ),
                        )
                        .await;
                }
                _ => {
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "Nothing is playing.")
                        .await;
                }
            }
        } else if cmd == "!!volume" {
            let arg = content.strip_prefix("!!volume").unwrap_or("").trim();
            if arg.is_empty() {
                let volume = self
                    .state
                    .volumes
                    .lock()
                    .await
                    .get(&guild_id)
                    .copied()
                    .unwrap_or(1.0);
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        format!("Volume: `{:.0}%`. Usage: `!!volume <0-200>`", volume * 100.0),
                    )
                    .await;
            } else if let Ok(percent) = arg.trim_end_matches('%').parse::<f32>() {
                if (0.0..=200.0).contains(&percent) {
                    let volume = percent / 100.0;
                    self.state.volumes.lock().await.insert(guild_id, volume);
                    let handle = {
                        let tracks = self.state.current_tracks.lock().await;
                        tracks.get(&guild_id).map(|t| t.handle.clone())
                    };
                    if let Some(handle) = handle {
                        let _ = handle.set_volume(volume);
                    }
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, format!("Volume set to `{percent:.0}%`."))
                        .await;
                } else {
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "Volume must be between 0 and 200.")
                        .await;
                }
            } else {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Usage: `!!volume <0-200>`")
                    .await;
            }
        } else if cmd == "!!skip" {
            if skip_current(&ctx, &self.state, guild_id, msg.channel_id).await {
                let _ = msg
                    .channel_id
                    .say(&ctx.http, "Skipped to next song.")
                    .await;
            } else {
                let _ = msg.channel_id.say(&ctx.http, "Nothing to skip.").await;
            }
        } else if cmd == "!!clear" {
            if clear_queue(&self.state, guild_id).await {
                let _ = msg.channel_id.say(&ctx.http, "Queue cleared.").await;
            } else {
                let _ = msg.channel_id.say(&ctx.http, "No queue to clear.").await;
            }
        } else if cmd == "!!pause" {
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
        } else if cmd == "!!resume" {
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
        } else if cmd == "!!loop" {
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
        } else if cmd == "!!stop" {
            let manager = songbird::get(&ctx)
                .await
                .expect("Songbird Voice client placed in at initialisation.");

            if manager.get(guild_id).is_some() {
                stop_playback(&self.state, guild_id).await;
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
        } else if cmd == "!!stay" {
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
                    match manager.join(guild_id, connect_to).await {
                        Ok(call) => {
                            let _ = call.lock().await.deafen(true).await;
                        }
                        Err(err) => {
                            let _ = msg
                                .channel_id
                                .say(&ctx.http, format!("Error joining voice channel: {err}"))
                                .await;
                            return;
                        }
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
        } else if cmd == "!!disconnect" {
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
        } else if cmd == "!!cache" {
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
        } else if cmd == "!!help" {
            let help_text = "\
## **Offside BOT**
### **Playback**
• `!!play <song | URL>` — Play a song or add to queue (YouTube, Spotify, Apple Music, Deezer)
• `!!pause` — Pause current playback
• `!!resume` — Resume playback
• `!!skip` — Skip to the next song
• `!!stop` — Stop playback and clear the queue
• `!!loop` — Toggle loop mode for the current track
• `!!nowplaying` (`!!np`) — Show the current song and elapsed time
• `!!volume <0-200>` — Set playback volume
### **Queue**
• `!!queue` — Show currently playing song and upcoming queue
• `!!remove <n>` — Remove a song from the queue
• `!!shuffle` — Shuffle the queue
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

