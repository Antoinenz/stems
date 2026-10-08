//! Picking what plays when nobody asked for anything: a song like the one
//! that just played, from the allowed pool, not played recently.
//!
//! Similarity uses what the analysis measured: genres/styles (most
//! weight), moods, tempo (half/double time counts as close), danceability
//! and era. The best few are drawn at random so it doesn't loop.

use std::collections::HashSet;

use rand::Rng;
use stems_library::Entry;

/// How similar `b` is to `a`, roughly 0 … 7.
pub fn similarity(a: &Entry, b: &Entry) -> f32 {
    let (Some(x), Some(y)) = (a.analysis.as_ref(), b.analysis.as_ref()) else {
        return 0.0;
    };
    // genres: shared styles, and shared parents at half weight
    let mut g = 0.0f32;
    for ga in &x.genres {
        for gb in &y.genres {
            if ga.name == gb.name {
                g += ga.score.min(gb.score);
            } else if parent(&ga.name) == parent(&gb.name) {
                g += 0.5 * ga.score.min(gb.score);
            }
        }
    }
    let g = (g * 2.0).min(1.0);
    // moods: 1 − mean difference over the moods both have
    let common: Vec<f32> = x
        .moods
        .iter()
        .filter_map(|(k, va)| y.moods.get(k).map(|vb| (va - vb).abs()))
        .collect();
    let m = if common.is_empty() {
        0.5
    } else {
        1.0 - common.iter().sum::<f32>() / common.len() as f32
    };
    // tempo: distance in octaves, half/double time counted as close
    let t = match (x.bpm, y.bpm) {
        (Some(p), Some(q)) if p > 0.0 && q > 0.0 => {
            let d = (p / q).log2();
            let d = d.abs().min((d - 1.0).abs()).min((d + 1.0).abs());
            (-(d / 0.1).powi(2)).exp()
        }
        _ => 0.5,
    };
    let dance = match (x.danceability, y.danceability) {
        (Some(p), Some(q)) => 1.0 - (p - q).abs(),
        _ => 0.5,
    };
    let era = match (a.info.year(), b.info.year()) {
        (Some(p), Some(q)) => (-(p.abs_diff(q) as f32) / 8.0).exp(),
        _ => 0.5,
    };
    3.0 * g + 1.5 * m + 1.0 * t + 1.0 * dance + 0.7 * era
}

fn parent(name: &str) -> &str {
    name.split(" / ").next().unwrap_or(name)
}

/// The next song: playable, in `pool` (when given), not in `avoid`, like
/// `seed` (when there is one). Falls back to ignoring `avoid` when it
/// rules everything out.
pub fn pick<'a>(
    entries: &'a [Entry],
    seed: Option<&Entry>,
    pool: Option<&HashSet<String>>,
    avoid: &HashSet<&str>,
    rng: &mut impl Rng,
) -> Option<&'a Entry> {
    let allowed: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.is_playable())
        .filter(|e| pool.is_none_or(|p| p.contains(&e.key)))
        .collect();
    let fresh: Vec<&Entry> = allowed
        .iter()
        .copied()
        .filter(|e| !avoid.contains(e.key.as_str()))
        .collect();
    let candidates = if fresh.is_empty() { allowed } else { fresh };
    if candidates.is_empty() {
        return None;
    }
    let Some(seed) = seed.filter(|s| s.analysis.is_some()) else {
        return Some(candidates[rng.random_range(0..candidates.len())]);
    };
    let mut scored: Vec<(f32, &Entry)> = candidates
        .into_iter()
        .filter(|e| e.key != seed.key)
        .map(|e| (similarity(seed, e), e))
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    // the best few, weighted towards the top
    let top = &scored[..scored.len().min(8)];
    let weights: Vec<f32> = top.iter().map(|(s, _)| (s + 0.1).powi(2)).collect();
    let total: f32 = weights.iter().sum();
    let mut r = rng.random_range(0.0..total.max(f32::MIN_POSITIVE));
    for ((_, e), w) in top.iter().zip(&weights) {
        if r < *w {
            return Some(e);
        }
        r -= w;
    }
    Some(top[0].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::{TrackId, TrackRef};
    use stems_library::{Analysis, Genre, Stage};

    fn song(id: &str, genre: &str, bpm: f32, dance: f32, year: &str) -> Entry {
        let t = TrackRef {
            id: TrackId::Spotify(id.into()),
            isrc: None,
            title: id.into(),
            artists: vec![],
            album: None,
            duration_ms: 1,
            art: None,
        };
        let mut e = Entry::new(format!("spotify_{id}"), t, 0);
        e.stages.audio = Stage::Done;
        e.stages.stems = Stage::Done;
        e.info.release_date = Some(year.into());
        e.analysis = Some(Analysis {
            bpm: Some(bpm),
            danceability: Some(dance),
            genres: vec![Genre {
                name: genre.into(),
                score: 0.6,
            }],
            moods: [("party".to_string(), dance)].into(),
            ..Default::default()
        });
        e
    }

    #[test]
    fn similar_songs_score_higher() {
        let house = song("a", "Electronic / House", 124.0, 0.9, "2005");
        let house2 = song("b", "Electronic / House", 126.0, 0.85, "2008");
        let techno = song("c", "Electronic / Techno", 130.0, 0.8, "2010");
        let ballad = song("d", "Rock / Soft Rock", 70.0, 0.2, "1978");
        let double = song("e", "Electronic / House", 62.0, 0.9, "2005");
        assert!(similarity(&house, &house2) > similarity(&house, &techno));
        assert!(similarity(&house, &techno) > similarity(&house, &ballad));
        // 62 bpm is half time of 124: close
        assert!(similarity(&house, &double) > similarity(&house, &techno));
    }

    #[test]
    fn picks_from_the_pool_and_avoids_recent() {
        let songs = vec![
            song("a", "Electronic / House", 124.0, 0.9, "2005"),
            song("b", "Electronic / House", 126.0, 0.85, "2008"),
            song("c", "Rock / Soft Rock", 70.0, 0.2, "1978"),
        ];
        let mut rng = rand::rng();
        let pool: HashSet<String> = ["spotify_b".into(), "spotify_c".into()].into();
        for _ in 0..20 {
            let p = pick(
                &songs,
                Some(&songs[0]),
                Some(&pool),
                &HashSet::new(),
                &mut rng,
            )
            .unwrap();
            assert_ne!(p.key, "spotify_a", "outside the pool");
        }
        let avoid: HashSet<&str> = ["spotify_b"].into();
        let p = pick(&songs, Some(&songs[0]), None, &avoid, &mut rng).unwrap();
        assert_eq!(p.key, "spotify_c", "b was played recently, a is the seed");
        // everything avoided: still plays something
        let all: HashSet<&str> = ["spotify_a", "spotify_b", "spotify_c"].into();
        assert!(pick(&songs, None, None, &all, &mut rng).is_some());
    }
}
