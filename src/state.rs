use std::{
    collections::{HashMap, VecDeque},
    env,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use regex::Regex;
use serenity::model::id::{GuildId, UserId};
use songbird::tracks::PlayMode;
use tokio::{sync::Mutex, task::AbortHandle};

use crate::{
    cache::SongCache,
    models::{ActiveTrack, LoopMode, QueueEntry, SongInfo, StayMode},
};

pub struct BotState {
    pub queues: Mutex<HashMap<GuildId, VecDeque<QueueEntry>>>,
    pub loop_modes: Mutex<HashMap<GuildId, LoopMode>>,
    pub stay_modes: Mutex<HashMap<GuildId, StayMode>>,
    pub current_songs: Mutex<HashMap<GuildId, SongInfo>>,
    pub current_tracks: Mutex<HashMap<GuildId, ActiveTrack>>,
    pub disconnect_timers: Mutex<HashMap<GuildId, AbortHandle>>,
    pub bot_user_id: Mutex<Option<UserId>>,
    pub play_locks: Mutex<HashMap<GuildId, Arc<Mutex<()>>>>,
    pub volumes: Mutex<HashMap<GuildId, f32>>,
    pub next_ticket: AtomicU64,
    pub http_client: reqwest::Client,
    pub spotify_regex: Regex,
    pub apple_music_regex: Regex,
    pub deezer_regex: Regex,
    pub youtube_id_regex: Regex,
    pub cache: Arc<SongCache>,
    pub is_dev_instance: bool,
    pub dev_mode: AtomicBool,
}

impl BotState {
    pub fn new(is_dev_instance: bool) -> Self {
        let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "cache".to_string());
        Self {
            queues: Mutex::new(HashMap::new()),
            loop_modes: Mutex::new(HashMap::new()),
            stay_modes: Mutex::new(HashMap::new()),
            current_songs: Mutex::new(HashMap::new()),
            current_tracks: Mutex::new(HashMap::new()),
            disconnect_timers: Mutex::new(HashMap::new()),
            bot_user_id: Mutex::new(None),
            play_locks: Mutex::new(HashMap::new()),
            volumes: Mutex::new(HashMap::new()),
            next_ticket: AtomicU64::new(1),
            http_client: reqwest::Client::new(),
            spotify_regex: Regex::new(r"https?://open\.spotify\.com/track/([a-zA-Z0-9]+)")
                .expect("Valid Spotify regex"),
            apple_music_regex: Regex::new(r"[?&]i=(\d+)|/song/(?:[^/]+/)?(\d+)")
                .expect("Valid Apple Music regex"),
            deezer_regex: Regex::new(r"deezer\.com/(?:[a-zA-Z-]+/)?track/(\d+)")
                .expect("Valid Deezer regex"),
            youtube_id_regex: Regex::new(r"(?:v=|youtu\.be/)([a-zA-Z0-9_-]{11})")
                .expect("Valid YouTube ID regex"),
            cache: Arc::new(SongCache::new(cache_dir)),
            is_dev_instance,
            // When you launch `cargo run` locally, dev_mode starts true on the local instance;
            // on the production container, dev_mode starts false until `!!dev on` is sent.
            dev_mode: AtomicBool::new(is_dev_instance),
        }
    }

    pub fn should_handle_commands(&self) -> bool {
        let dev_enabled = self.dev_mode.load(Ordering::SeqCst);
        if self.is_dev_instance {
            dev_enabled
        } else {
            !dev_enabled
        }
    }

    /// Per-guild lock serializing every change to "what is playing" (play, skip, track end, stop).
    pub async fn guild_lock(&self, guild_id: GuildId) -> Arc<Mutex<()>> {
        self.play_locks
            .lock()
            .await
            .entry(guild_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn cancel_disconnect_timer(&self, guild_id: GuildId) {
        if let Some(abort_handle) = self.disconnect_timers.lock().await.remove(&guild_id) {
            abort_handle.abort();
        }
    }

    pub async fn cleanup_guild(&self, guild_id: GuildId) {
        if let Some(active) = self.current_tracks.lock().await.remove(&guild_id) {
            active.suppress_end.store(true, Ordering::SeqCst);
            let _ = active.handle.stop();
        }
        self.queues.lock().await.remove(&guild_id);
        self.current_songs.lock().await.remove(&guild_id);
        self.loop_modes.lock().await.remove(&guild_id);
        self.stay_modes.lock().await.remove(&guild_id);
        self.volumes.lock().await.remove(&guild_id);
        self.play_locks.lock().await.remove(&guild_id);
        self.cancel_disconnect_timer(guild_id).await;
    }

    pub async fn is_playing(&self, guild_id: GuildId) -> bool {
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

    pub async fn is_paused(&self, guild_id: GuildId) -> bool {
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
}
