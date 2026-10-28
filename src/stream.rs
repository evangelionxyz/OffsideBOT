use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Child, Command as StdCommand, Stdio},
    sync::{Arc, Condvar, Mutex as StdMutex},
};

use anyhow::{anyhow, Result};
use songbird::input::{
    core::io::{MediaSource, ReadOnlySource},
    AudioStream, Input, LiveInput,
};

use crate::{
    cache::{now_unix_secs, SongCache, MAX_CACHE_BYTES},
    models::SongInfo,
};

pub fn ytdl_program() -> &'static str {
    if Path::new("yt-dlp.exe").exists() {
        "./yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

pub fn spawn_ytdl_child(url: &str) -> Result<Child> {
    let mut cmd = StdCommand::new(ytdl_program());
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
        .stderr(Stdio::null());

    cmd.spawn()
        .map_err(|e| anyhow!("Failed to spawn yt-dlp: {e}"))
}

struct SharedTeeBuffer {
    buf: VecDeque<u8>,
    eof: bool,
    error: Option<String>,
    reader_dropped: bool,
}

struct TeeStreamReader {
    shared: Arc<(StdMutex<SharedTeeBuffer>, Condvar)>,
}

impl Read for TeeStreamReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap();
        loop {
            if !guard.buf.is_empty() {
                let n = out.len().min(guard.buf.len());
                for b in out.iter_mut().take(n) {
                    *b = guard.buf.pop_front().unwrap();
                }
                return Ok(n);
            }
            if let Some(ref err) = guard.error {
                return Err(std::io::Error::other(err.clone()));
            }
            if guard.eof {
                return Ok(0);
            }
            guard = cvar.wait(guard).unwrap();
        }
    }
}

impl Drop for TeeStreamReader {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        if let Ok(mut guard) = lock.lock() {
            guard.reader_dropped = true;
            guard.buf.clear();
            cvar.notify_all();
        }
    }
}

/// Streams audio from `yt-dlp` into Songbird immediately while simultaneously
/// saving the complete audio stream into the disk cache (`cache/<id>.audio`).
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

    let shared = Arc::new((
        StdMutex::new(SharedTeeBuffer {
            buf: VecDeque::with_capacity(64 * 1024),
            eof: false,
            error: None,
            reader_dropped: false,
        }),
        Condvar::new(),
    ));

    let shared_writer = shared.clone();
    let rt_handle = tokio::runtime::Handle::current();

    std::thread::spawn(move || {
        let mut part_file = File::create(&part_path).ok();
        let mut chunk = [0u8; 32 * 1024];
        let mut total_written: u64 = 0;
        let mut download_complete = false;

        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    download_complete = true;
                    break;
                }
                Ok(n) => {
                    if let Some(ref mut f) = part_file {
                        if total_written + (n as u64) <= MAX_CACHE_BYTES {
                            if f.write_all(&chunk[..n]).is_err() {
                                part_file = None;
                            } else {
                                total_written += n as u64;
                            }
                        } else {
                            part_file = None;
                        }
                    }

                    let (lock, cvar) = &*shared_writer;
                    let mut guard = lock.lock().unwrap();
                    if !guard.reader_dropped {
                        guard.buf.extend(&chunk[..n]);
                        cvar.notify_all();
                    }
                }
                Err(e) => {
                    let (lock, cvar) = &*shared_writer;
                    let mut guard = lock.lock().unwrap();
                    guard.error = Some(e.to_string());
                    cvar.notify_all();
                    break;
                }
            }
        }

        let status_ok = child.wait().map(|s| s.success()).unwrap_or(false);
        {
            let (lock, cvar) = &*shared_writer;
            let mut guard = lock.lock().unwrap();
            guard.eof = true;
            cvar.notify_all();
        }

        drop(part_file);

        if download_complete && status_ok && total_written > 0 {
            if fs::rename(&part_path, &final_path).is_ok() {
                rt_handle.spawn(async move {
                    cache.insert_cached_file(&song, None, final_file_name).await;
                });
                return;
            }
        }

        let _ = fs::remove_file(&part_path);
    });

    let reader = TeeStreamReader { shared };
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

        let safe_id = SongCache::sanitize_id(&song.id);
        let final_file_name = format!("{safe_id}.audio");
        let part_file_name = format!("{safe_id}.prefetch.{}.part", now_unix_secs());
        let part_path = cache.file_path(&part_file_name);
        let final_path = cache.file_path(&final_file_name);

        let mut cmd = tokio::process::Command::new(ytdl_program());
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
            .stderr(Stdio::null());

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

