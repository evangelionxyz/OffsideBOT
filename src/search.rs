use std::path::Path;

use anyhow::{anyhow, Result};

use crate::{models::SongInfo, state::BotState, stream::ytdl_program};

#[derive(Clone, Debug)]
pub struct SearchCandidate {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<u64>,
    pub url: String,
}

pub async fn fetch_search_candidates(target: &str) -> Result<Vec<SearchCandidate>> {
    let mut cmd = tokio::process::Command::new(ytdl_program());
    cmd.args([
        "--flat-playlist",
        "--print",
        "%(id)s\t%(title)s\t%(channel,uploader)s\t%(duration)s\t%(webpage_url,url)s",
        "--no-playlist",
        "--no-warnings",
        "--no-check-certificate",
    ]);
    if Path::new("cookies.txt").exists() {
        cmd.args(["--cookies", "cookies.txt"]);
    }
    cmd.arg(target);

    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("Could not run yt-dlp: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp error: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut candidates = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let id = parts[0].trim().to_string();
        if id.is_empty() || id == "NA" {
            continue;
        }
        let title = parts[1].trim().to_string();
        if title.is_empty() || title == "NA" {
            continue;
        }
        let channel = parts.get(2).map(|s| s.trim()).unwrap_or("").to_string();
        let duration = parts.get(3).and_then(|s| s.trim().parse::<u64>().ok());
        let url = parts
            .get(4)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != "NA")
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));

        candidates.push(SearchCandidate {
            id,
            title,
            channel,
            duration,
            url,
        });
    }

    Ok(candidates)
}

pub fn score_candidate(
    candidate: &SearchCandidate,
    rank: usize,
    query: &str,
    wants_bts: bool,
    wants_video: bool,
) -> i32 {
    let title_lower = candidate.title.to_lowercase();
    let channel_lower = candidate.channel.to_lowercase();
    let q_lower = query.to_lowercase();

    let mut score: i32 = 0;

    // 1. YouTube ranking preference (higher in search results gets a small head start)
    score += (10 - (rank.min(10) as i32)) * 2;

    // 2. Reject behind the scenes, bloopers, making-of, extra footage, interviews, reactions
    if !wants_bts {
        let bts_markers = [
            "behind the scene",
            "behind the scenes",
            "making of",
            "extra footage",
            "deleted scene",
            "blooper",
            "bloopers",
            "outtake",
            "outtakes",
            "b-roll",
            "interview",
            "reaction",
            "reacts",
            "review",
            "short film",
            "teaser",
            "trailer",
        ];
        if bts_markers.iter().any(|&m| title_lower.contains(m))
            || title_lower.contains("(bts)")
            || title_lower.contains("[bts]")
            || title_lower.contains(" bts ")
            || title_lower.starts_with("bts ")
            || title_lower.ends_with(" bts")
        {
            return -10000;
        }
    }

    // 3. Official music video penalty when user just wants the audio track
    // (Music videos frequently contain talking, dialogue, intro skits, movie interludes, gunfights)
    if !wants_video {
        if title_lower.contains("official music video")
            || title_lower.contains("official video")
            || title_lower.contains("music video")
            || title_lower.contains("(mv)")
            || title_lower.contains("[mv]")
        {
            score -= 40;
        }
    }

    // 4. Boost audio-only and clean music releases
    if channel_lower.ends_with("- topic") || channel_lower.contains("- topic") {
        // YouTube's official label track (Topic channel)
        score += 60;
    }
    if title_lower.contains("official audio") {
        score += 50;
    } else if title_lower.contains("audio") || title_lower.contains("(audio)") || title_lower.contains("[audio]") {
        score += 35;
    }

    if title_lower.contains("official lyric video") || title_lower.contains("lyric video") || title_lower.contains("lyrics") {
        score += 30;
    }

    if title_lower.contains("visualizer") || title_lower.contains("official visualizer") {
        score += 25;
    }

    if title_lower.contains("album version") || title_lower.contains("original mix") || title_lower.contains("studio version") || title_lower.contains("clean") {
        score += 15;
    }

    // 5. Duration sanity checks
    if let Some(dur) = candidate.duration {
        // Less than 30s is likely a snippet, teaser, or ringtone
        if dur < 30 {
            score -= 100;
        }

        let is_long_format_query = q_lower.contains("hour")
            || q_lower.contains("mix")
            || q_lower.contains("compilation")
            || q_lower.contains("podcast")
            || q_lower.contains("full album")
            || q_lower.contains("lofi")
            || q_lower.contains("lo-fi")
            || q_lower.contains("chill")
            || q_lower.contains("relax")
            || q_lower.contains("study")
            || q_lower.contains("bgm")
            || q_lower.contains("stream");

        if !is_long_format_query {
            if dur > 900 {
                score -= 500;
            } else if dur > 600 {
                score -= 100;
            }
        }
    }

    // 6. Query keyword match relevance
    let query_words: Vec<&str> = q_lower
        .split_whitespace()
        .filter(|w| w.len() > 1 && *w != "audio" && *w != "the" && *w != "and" && *w != "song")
        .collect();

    for word in &query_words {
        if title_lower.contains(word) || channel_lower.contains(word) {
            score += 15;
        }
    }

    score
}

pub async fn extract_song_info(state: &BotState, query: &str) -> Result<SongInfo> {
    // 1. Check if this exact query or URL is already in the 7-day cache
    if let Some((cached_song, _path)) = state.cache.get_by_query(query).await {
        return Ok(cached_song);
    }

    // Also check if query is a direct YouTube URL whose 11-char video ID is already cached
    if let Some(caps) = state.youtube_id_regex.captures(query) {
        if let Some(id_match) = caps.get(1) {
            let video_id = id_match.as_str();
            if state.cache.get_cached_path(video_id).await.is_some() {
                let index = state.cache.index.lock().await;
                if let Some(entry) = index.entries.get(video_id) {
                    return Ok(SongInfo {
                        id: entry.id.clone(),
                        url: entry.url.clone(),
                        title: entry.title.clone(),
                    });
                }
            }
        }
    }

    let is_direct_url = query.starts_with("http://") || query.starts_with("https://");

    if is_direct_url {
        let candidates = fetch_search_candidates(query).await?;
        let first = candidates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No results found for URL."))?;

        let final_url = if first.url.starts_with("http://") || first.url.starts_with("https://") {
            first.url
        } else {
            format!("https://www.youtube.com/watch?v={}", first.id)
        };

        let song = SongInfo {
            id: first.id,
            url: final_url,
            title: first.title,
        };

        state.cache.remember_query(query, &song.id).await;
        return Ok(song);
    }

    let q_lower = query.to_lowercase();
    let wants_bts = q_lower.contains("behind the scene")
        || q_lower.contains("bts")
        || q_lower.contains("making of")
        || q_lower.contains("extra footage")
        || q_lower.contains("blooper")
        || q_lower.contains("deleted scene")
        || q_lower.contains("interview");

    let wants_video = q_lower.contains("video")
        || q_lower.contains("mv")
        || q_lower.contains("music video")
        || q_lower.contains("short film");

    let already_audio_focused = q_lower.contains("audio")
        || q_lower.contains("lyrics")
        || q_lower.contains("lyric")
        || q_lower.contains("topic");

    // First attempt: fine-tuned audio search if user did not request BTS or video explicitly
    let target = if wants_bts || wants_video || already_audio_focused {
        format!("ytsearch10:{query}")
    } else {
        format!("ytsearch10:{query} audio")
    };

    let candidates = fetch_search_candidates(&target).await.unwrap_or_default();

    let mut scored: Vec<(i32, SearchCandidate)> = candidates
        .into_iter()
        .enumerate()
        .map(|(rank, c)| {
            let score = score_candidate(&c, rank, query, wants_bts, wants_video);
            (score, c)
        })
        .filter(|(score, _)| *score > -5000)
        .collect();

    // If searching with "audio" returned no valid candidates, fall back to plain query
    if scored.is_empty() && !wants_bts && !wants_video && !already_audio_focused {
        let fallback_target = format!("ytsearch10:{query}");
        if let Ok(fallback_candidates) = fetch_search_candidates(&fallback_target).await {
            scored = fallback_candidates
                .into_iter()
                .enumerate()
                .map(|(rank, c)| {
                    let score = score_candidate(&c, rank, query, wants_bts, wants_video);
                    (score, c)
                })
                .filter(|(score, _)| *score > -5000)
                .collect();
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0));

    let best = scored
        .into_iter()
        .next()
        .map(|(_, c)| c)
        .ok_or_else(|| anyhow!("No results found."))?;

    let final_title = if !best.channel.is_empty()
        && best.channel != "NA"
        && (best.channel.to_lowercase().ends_with("- topic") || best.channel.to_lowercase().contains("- topic"))
    {
        let clean_artist = best
            .channel
            .replace("- Topic", "")
            .replace("- topic", "")
            .trim()
            .to_string();
        if !clean_artist.is_empty() && !best.title.to_lowercase().contains(&clean_artist.to_lowercase()) {
            format!("{clean_artist} - {}", best.title)
        } else {
            best.title
        }
    } else {
        best.title
    };

    let final_url = format!("https://www.youtube.com/watch?v={}", best.id);

    state.cache.remember_query(query, &best.id).await;

    Ok(SongInfo {
        id: best.id,
        url: final_url,
        title: final_title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_audio_search_thriller() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Michael Jackson Thriller").await.unwrap();
        println!("Resolved Thriller: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the 14-minute official 4K music video (id: sOnqjkJTMaA)
        assert_ne!(res.id, "sOnqjkJTMaA", "Must not pick the music video with movie intro/extra footage");
    }

    #[tokio::test]
    async fn test_audio_search_paparazzi() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Lady Gaga Paparazzi").await.unwrap();
        println!("Resolved Paparazzi: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the 7-minute music video with skits (d2smz_1L2_0 / qStFzmQGQNw)
        assert_ne!(res.id, "d2smz_1L2_0");
        assert_ne!(res.id, "qStFzmQGQNw");
    }

    #[tokio::test]
    async fn test_audio_search_payphone() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Maroon 5 Payphone").await.unwrap();
        println!("Resolved Payphone: id={}, title={}, url={}", res.id, res.title, res.url);
        // Ensure it did not pick the music video with bank robbery gunfight (id: KRaWnd3LJfs)
        assert_ne!(res.id, "KRaWnd3LJfs");
    }

    #[tokio::test]
    async fn test_direct_url_preserved() {
        let state = BotState::new(true);
        let url = "https://www.youtube.com/watch?v=sOnqjkJTMaA";
        let res = extract_song_info(&state, url).await.unwrap();
        assert_eq!(res.id, "sOnqjkJTMaA");
    }

    #[tokio::test]
    async fn test_explicit_bts_request() {
        let state = BotState::new(true);
        let res = extract_song_info(&state, "Michael Jackson Thriller making of behind the scenes").await.unwrap();
        println!("Resolved explicit BTS: id={}, title={}", res.id, res.title);
        let lower = res.title.to_lowercase();
        assert!(lower.contains("making") || lower.contains("behind") || lower.contains("thriller"));
    }
}

