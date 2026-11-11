use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::Path,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex as StdMutex},
};

use anyhow::{Result, anyhow};
use songbird::input::{
    AudioStream, Input, LiveInput,
    core::io::{MediaSource, ReadOnlySource},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc,
};

use crate::{
    cache::{MAX_CACHE_BYTES, SongCache, now_unix_secs},
    models::SongInfo,
};

/// Song IDs currently being downloaded, so the prefetcher and the live stream never
/// write the same cache file at the same time.
static INFLIGHT: LazyLock<StdMutex<HashSet<String>>> =
    LazyLock::new(|| StdMutex::new(HashSet::new()));

static PREFETCH_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub struct InflightGuard(String);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = INFLIGHT.lock() {
            set.remove(&self.0);
        }
    }
}

/// Returns a guard if no other download for this song is running.
pub fn try_begin_download(id: &str) -> Option<InflightGuard> {
    let mut set = INFLIGHT.lock().ok()?;
    if set.insert(id.to_string()) {
        Some(InflightGuard(id.to_string()))
    } else {
        None
    }
}

pub fn ytdl_program() -> &'static str {
    if Path::new("yt-dlp.exe").exists() {
        "./yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

pub fn spawn_ytdl_child(url: &str) -> Result<Child> {
    let mut cmd = Command::new(ytdl_program());
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
        .stderr(Stdio::null())
        .kill_on_drop(true);

    cmd.spawn()
        .map_err(|e| anyhow!("Failed to spawn yt-dlp: {e}"))
}

/// Sync `Read` adapter over a bounded channel, consumed by Songbird's audio thread.
struct ChannelReader {
    rx: mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Some(Err(err)) => return Err(std::io::Error::other(err)),
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.current.len() - self.pos);
        out[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Streams audio from `yt-dlp` into Songbird immediately while simultaneously
/// saving the complete audio stream into the disk cache (`cache/<id>.audio`).
/// Must be called from within a Tokio runtime.
pub fn spawn_tee_cached_stream(song: SongInfo, cache: Arc<SongCache>) -> Result<Input> {
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

    // If someone else is already caching this song, stream live without writing the cache.
    let guard = try_begin_download(&song.id);

    let (tx, rx) = mpsc::channel::<std::result::Result<Vec<u8>, String>>(32);

    tokio::spawn(async move {
        let _guard = guard;
        let mut part_file = if _guard.is_some() {
            tokio::fs::File::create(&part_path).await.ok()
        } else {
            None
        };
        let mut chunk = vec![0u8; 32 * 1024];
        let mut total_written: u64 = 0;
        let mut download_complete = false;
        let mut listener_alive = true;

        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) => {
                    download_complete = true;
                    break;
                }
                Ok(n) => {
                    if let Some(f) = part_file.as_mut() {
                        if total_written + (n as u64) <= MAX_CACHE_BYTES
                            && f.write_all(&chunk[..n]).await.is_ok()
                        {
                            total_written += n as u64;
                        } else {
                            part_file = None;
                        }
                    }
                    if listener_alive && tx.send(Ok(chunk[..n].to_vec())).await.is_err() {
                        listener_alive = false;
                    }
                    // Nobody is listening and nothing is being cached: stop downloading.
                    if !listener_alive && part_file.is_none() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
        drop(tx);

        if !download_complete {
            let _ = child.kill().await;
        }
        let status_ok = child.wait().await.map(|s| s.success()).unwrap_or(false);

        if let Some(mut f) = part_file.take() {
            let _ = f.flush().await;
        }

        if download_complete && status_ok && total_written > 0 {
            if fs::rename(&part_path, &final_path).is_ok() {
                cache.insert_cached_file(&song, None, final_file_name).await;
                return;
            }
        }
        let _ = fs::remove_file(&part_path);
    });

    let reader = ChannelReader {
        rx,
        current: Vec::new(),
        pos: 0,
    };
    let audio_stream = AudioStream {
        input: Box::new(ReadOnlySource::new(reader)) as Box<dyn MediaSource>,
    };
    Ok(Input::Live(LiveInput::Raw(audio_stream), None))
}

/// Pre-downloads a queued song in the background so it is ready on disk before its turn.
pub fn prefetch_song_in_background(song: SongInfo, cache: Arc<SongCache>) {
    tokio::spawn(async move {
        if cache.get_cached_path(&song.id).await.is_some() {
            return;
        }
        let Some(_guard) = try_begin_download(&song.id) else {
            return;
        };
        // Never run more than one background download at a time.
        let Ok(_permit) = PREFETCH_SLOTS.acquire().await else {
            return;
        };

        let safe_id = SongCache::sanitize_id(&song.id);
        let final_file_name = format!("{safe_id}.audio");
        let part_file_name = format!("{safe_id}.prefetch.{}.part", now_unix_secs());
        let part_path = cache.file_path(&part_file_name);
        let final_path = cache.file_path(&final_file_name);

        let mut cmd = Command::new(ytdl_program());
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
            .stderr(Stdio::null())
            .kill_on_drop(true);

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
