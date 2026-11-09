use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use serenity::{
    async_trait,
    client::Context,
    model::id::{ChannelId, GuildId},
};
use songbird::{
    input::{
        codecs::{get_codec_registry, get_probe},
        Input,
    },
    tracks::PlayMode,
    Event, EventContext, EventHandler as VoiceEventHandler, TrackEvent,
};

use crate::{
    models::{ActiveTrack, QueueEntry, SongInfo, StayMode},
    state::BotState,
    stream::{prefetch_song_in_background, spawn_tee_cached_stream},
};

pub struct TrackEndNotifier {
    pub ctx: Context,
    pub state: Arc<BotState>,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub suppress_end: Arc<AtomicBool>,
    pub track_uuid: String,
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
        let track_uuid = self.track_uuid.clone();

        tokio::spawn(async move {
            handle_track_end(&ctx, &state, guild_id, channel_id, &track_uuid).await;
        });

        None
    }
}

pub fn start_disconnect_timer(
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

pub const MAX_QUEUE_LEN: usize = 100;

async fn build_input(state: &Arc<BotState>, song: &SongInfo) -> Result<Input> {
    // Play straight from the disk cache when possible (streamed from disk, not loaded into RAM),
    // otherwise stream live from yt-dlp while teeing into the cache.
    if let Some(cached_path) = state.cache.get_cached_path(&song.id).await {
        let non_empty = tokio::fs::metadata(&cached_path)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if non_empty {
            let input: Input = songbird::input::File::new(cached_path).into();
            if let Ok(playable) = input
                .make_playable_async(get_codec_registry(), get_probe())
                .await
            {
                return Ok(playable);
            }
        }
    }

    spawn_tee_cached_stream(song.clone(), state.cache.clone())?
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|e| anyhow!("Failed to prepare audio stream: {e}"))
}

pub async fn play_song_on_call(
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

    let input = build_input(state, song).await?;

    let track_handle = {
        let mut handler = handler_lock.lock().await;
        handler.play_input(input)
    };

    if let Some(volume) = state.volumes.lock().await.get(&guild_id).copied() {
        let _ = track_handle.set_volume(volume);
    }

    let suppress_end = Arc::new(AtomicBool::new(false));
    let _ = track_handle.add_event(
        Event::Track(TrackEvent::End),
        TrackEndNotifier {
            ctx: ctx.clone(),
            state: state.clone(),
            guild_id,
            channel_id,
            suppress_end: suppress_end.clone(),
            track_uuid: track_handle.uuid().to_string(),
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

/// Called when a track ends on its own. Only advances if the ended track is still the
/// current one, so stale or duplicate end events can never skip a song.
async fn handle_track_end(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
    track_uuid: &str,
) {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let is_current = state
        .current_tracks
        .lock()
        .await
        .get(&guild_id)
        .map(|t| t.handle.uuid().to_string() == track_uuid)
        .unwrap_or(false);
    if !is_current {
        return;
    }

    play_next_locked(ctx, state, guild_id, channel_id).await;
}

pub enum FillResult {
    Started,
    Queued(usize),
    /// The slot was removed (e.g. `!!clear`) before the search finished.
    Cancelled,
}

/// Reserves a queue slot immediately so queue order follows request order,
/// even though searches finish at different times. Returns `(ticket, position)`.
pub async fn reserve_slot(
    state: &Arc<BotState>,
    guild_id: GuildId,
    query: &str,
) -> std::result::Result<(u64, usize), String> {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let mut queues = state.queues.lock().await;
    let queue = queues.entry(guild_id).or_default();
    if queue.len() >= MAX_QUEUE_LEN {
        return Err(format!("Queue is full (max {MAX_QUEUE_LEN} songs)."));
    }
    let ticket = state.next_ticket.fetch_add(1, Ordering::SeqCst);
    queue.push_back(QueueEntry {
        ticket,
        query: query.to_string(),
        song: None,
    });
    let position = queue.len();
    drop(queues);

    state.cancel_disconnect_timer(guild_id).await;
    Ok((ticket, position))
}

/// Fills a reserved slot with the resolved song and starts playback if the bot is idle.
pub async fn fill_slot(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
    ticket: u64,
    song: SongInfo,
) -> FillResult {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let position = {
        let mut queues = state.queues.lock().await;
        let Some(queue) = queues.get_mut(&guild_id) else {
            return FillResult::Cancelled;
        };
        let Some(index) = queue.iter().position(|e| e.ticket == ticket) else {
            return FillResult::Cancelled;
        };
        queue[index].song = Some(song.clone());
        index + 1
    };

    let idle = !state.current_tracks.lock().await.contains_key(&guild_id);
    if idle && position == 1 {
        play_next_locked(ctx, state, guild_id, channel_id).await;
        FillResult::Started
    } else {
        // Download in the background so it starts instantly when its turn comes
        prefetch_song_in_background(song, state.cache.clone());
        FillResult::Queued(position)
    }
}

/// Drops a reserved slot (failed search) and keeps the queue moving.
pub async fn cancel_slot(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
    ticket: u64,
) {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let remaining = {
        let mut queues = state.queues.lock().await;
        match queues.get_mut(&guild_id) {
            Some(queue) => {
                queue.retain(|e| e.ticket != ticket);
                queue.len()
            }
            None => 0,
        }
    };

    let idle = !state.current_tracks.lock().await.contains_key(&guild_id);
    if idle && remaining > 0 {
        play_next_locked(ctx, state, guild_id, channel_id).await;
    } else if idle {
        start_disconnect_timer(ctx.clone(), state.clone(), guild_id, channel_id);
    }
}

/// Skips the current song. Returns false if nothing was playing.
pub async fn skip_current(
    ctx: &Context,
    state: &Arc<BotState>,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> bool {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let Some(active) = state.current_tracks.lock().await.remove(&guild_id) else {
        return false;
    };
    active.suppress_end.store(true, Ordering::SeqCst);
    let _ = active.handle.stop();

    play_next_locked(ctx, state, guild_id, channel_id).await;
    true
}

/// Stops playback and clears the queue under the guild lock.
pub async fn stop_playback(state: &Arc<BotState>, guild_id: GuildId) {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    if let Some(active) = state.current_tracks.lock().await.remove(&guild_id) {
        active.suppress_end.store(true, Ordering::SeqCst);
        let _ = active.handle.stop();
    }
    if let Some(queue) = state.queues.lock().await.get_mut(&guild_id) {
        queue.clear();
    }
}

/// Clears waiting songs (including pending searches). Returns false if there was no queue.
pub async fn clear_queue(state: &Arc<BotState>, guild_id: GuildId) -> bool {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    match state.queues.lock().await.get_mut(&guild_id) {
        Some(queue) => {
            queue.clear();
            true
        }
        None => false,
    }
}

/// Removes the 1-based `position` from the queue.
pub async fn remove_from_queue(
    state: &Arc<BotState>,
    guild_id: GuildId,
    position: usize,
) -> Option<QueueEntry> {
    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let mut queues = state.queues.lock().await;
    let queue = queues.get_mut(&guild_id)?;
    if position == 0 || position > queue.len() {
        return None;
    }
    queue.remove(position - 1)
}

/// Shuffles the waiting songs. Returns how many were shuffled.
pub async fn shuffle_queue(state: &Arc<BotState>, guild_id: GuildId) -> usize {
    use std::hash::{BuildHasher, RandomState};

    let lock = state.guild_lock(guild_id).await;
    let _guard = lock.lock().await;

    let mut queues = state.queues.lock().await;
    let Some(queue) = queues.get_mut(&guild_id) else {
        return 0;
    };
    let hasher = RandomState::new();
    for i in (1..queue.len()).rev() {
        let j = (hasher.hash_one(i) as usize) % (i + 1);
        queue.swap(i, j);
    }
    queue.len()
}

/// Caller must hold the guild lock.
async fn play_next_locked(
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

    // Try queued songs in order; a song that fails to start must not stall the queue.
    loop {
        enum Front {
            Empty,
            Pending,
            Ready(SongInfo),
        }

        let front = {
            let mut queues = state.queues.lock().await;
            match queues.get_mut(&guild_id) {
                None => Front::Empty,
                Some(queue) => match queue.front() {
                    None => Front::Empty,
                    Some(entry) if entry.song.is_none() => Front::Pending,
                    Some(_) => {
                        let entry = queue.pop_front().expect("front checked above");
                        Front::Ready(entry.song.expect("song checked above"))
                    }
                },
            }
        };

        let next_song = match front {
            Front::Empty => break,
            Front::Pending => {
                // The next song is still being searched; `fill_slot` starts it when ready.
                state.current_tracks.lock().await.remove(&guild_id);
                return;
            }
            Front::Ready(song) => song,
        };

        state
            .current_songs
            .lock()
            .await
            .insert(guild_id, next_song.clone());

        if let Err(err) = play_song_on_call(ctx, state, guild_id, channel_id, &next_song).await {
            eprintln!("Error playing next song: {err:?}");
            let _ = channel_id.say(&ctx.http, format!("Error: {err}")).await;
            continue;
        }

        let _ = channel_id
            .say(&ctx.http, format!("Now playing: `{}`", next_song.title))
            .await;
        return;
    }

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
