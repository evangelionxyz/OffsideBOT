use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::sync::Mutex;

use crate::models::{CacheEntry, CacheIndex, SongInfo};

pub const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days
pub const MAX_CACHE_BYTES: u64 = 5 * 1024 * 1024 * 1024; // 5 GB

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct SongCache {
    pub dir: PathBuf,
    pub index_path: PathBuf,
    pub index: Mutex<CacheIndex>,
}

impl SongCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
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

    pub fn normalize_query(q: &str) -> String {
        q.trim().to_lowercase()
    }

    pub fn sanitize_id(id: &str) -> String {
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

    pub fn file_path(&self, file_name: &str) -> PathBuf {
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

    pub async fn enforce_limits(&self) {
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

    pub async fn get_by_query(&self, query: &str) -> Option<(SongInfo, PathBuf)> {
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

            // Verify that the cached song title actually matches the query
            let cached_title_words = crate::search::tokenize_words(&entry.title);
            if let Some(pos) = query.find(" - ") {
                let right = query[pos + 3..].trim();
                let title_words = crate::search::tokenize_words(right);
                let meaningful: Vec<&String> = title_words
                    .iter()
                    .filter(|w| (w.len() > 1 || w.chars().all(|c| c.is_numeric())) && !crate::search::STOP_WORDS.contains(&w.as_str()))
                    .collect();
                if !meaningful.is_empty() && !meaningful.iter().any(|w| cached_title_words.contains(w)) {
                    return None;
                }
            } else {
                let q_words = crate::search::tokenize_words(query);
                let meaningful: Vec<&String> = q_words
                    .iter()
                    .filter(|w| (w.len() > 1 || w.chars().all(|c| c.is_numeric())) && !crate::search::STOP_WORDS.contains(&w.as_str()))
                    .collect();
                if !meaningful.is_empty() && !meaningful.iter().any(|w| cached_title_words.contains(w)) {
                    return None;
                }
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

    pub async fn get_cached_path(&self, id: &str) -> Option<PathBuf> {
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

    pub async fn remember_query(&self, query: &str, id: &str) {
        let key = Self::normalize_query(query);
        let mut index = self.index.lock().await;
        index.queries.insert(key, id.to_string());
        self.save_index_locked(&index).await;
    }

    pub async fn insert_cached_file(&self, song: &SongInfo, query: Option<&str>, file_name: String) {
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

    pub async fn stats(&self) -> (usize, u64) {
        let index = self.index.lock().await;
        let count = index.entries.len();
        let bytes = index.entries.values().map(|e| e.size_bytes).sum();
        (count, bytes)
    }
}

