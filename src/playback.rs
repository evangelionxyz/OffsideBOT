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
    models::{ActiveTrack, SongInfo, StayMode},
    state::BotState,
    stream::spawn_tee_cached_stream,
};

pub struct TrackEndNotifier {
    pub ctx: Context,
    pub state: Arc<BotState>,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub suppress_end: Arc<AtomicBool>,
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

pub async fn play_next_song(
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

