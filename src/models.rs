use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
};

use serde::{Deserialize, Serialize};
use songbird::tracks::TrackHandle;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SongInfo {
    pub id: String,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub id: String,
    pub title: String,
    pub url: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub created_at: u64,
    pub last_accessed: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CacheIndex {
    pub entries: HashMap<String, CacheEntry>,
    pub queries: HashMap<String, String>,
}

/// A reserved queue slot. `song` is `None` while the search is still running,
/// which keeps queue order equal to request order.
#[derive(Clone, Debug)]
pub struct QueueEntry {
    pub ticket: u64,
    pub query: String,
    pub song: Option<SongInfo>,
}

impl QueueEntry {
    pub fn label(&self) -> String {
        match &self.song {
            Some(song) => song.title.clone(),
            None => format!("⏳ {}", self.query),
        }
    }
}

pub struct ActiveTrack {
    pub handle: TrackHandle,
    pub suppress_end: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum StayMode {
    #[default]
    Default,
    Hours(f64),
    Forever,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoopMode {
    #[default]
    Off,
    /// Repeat the last song once the queue runs out
    Track,
    /// Re-append every song to the back of the queue after it starts (playlist loop)
    Queue,
}
