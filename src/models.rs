use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
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

