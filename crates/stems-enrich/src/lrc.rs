//! LRC parser: `[mm:ss.xx]text`, multiple timestamps per line, metadata
//! tags ignored, and *enhanced* LRC word tags `<mm:ss.xx>word` inside a line.

use stems_core::{LyricLine, LyricWord};

pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            match parse_timestamp(&stripped[..end]) {
                Some(ms) => times.push(ms),
                None => {
                    if times.is_empty() {
                        // metadata tag such as [ar:Artist]; skip the line
                        rest = "";
                        break;
                    }
                }
            }
            rest = &stripped[end + 1..];
        }
        if times.is_empty() {
            continue;
        }
        let (line_text, words) = parse_words(rest.trim());
        for ms in times {
            out.push(LyricLine {
                time_ms: ms,
                text: line_text.clone(),
                words: words.clone(),
            });
        }
    }
    out.sort_by_key(|l| l.time_ms);
    out
}

/// Split `<mm:ss.xx>word <mm:ss.xx>word` into plain text plus word timings.
/// Lines without any `<…>` tags yield an empty word list.
fn parse_words(s: &str) -> (String, Vec<LyricWord>) {
    if !s.contains('<') {
        return (s.to_string(), Vec::new());
    }
    let mut text = String::new();
    let mut words = Vec::new();
    let mut rest = s;
    let mut current: Option<(u64, String)> = None;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('<') {
            if let Some(end) = after.find('>') {
                if let Some(ms) = parse_timestamp(&after[..end]) {
                    if let Some((t, w)) = current.take() {
                        push_word(&mut words, &mut text, t, w);
                    }
                    current = Some((ms, String::new()));
                    rest = &after[end + 1..];
                    continue;
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        match &mut current {
            Some((_, w)) => w.push(ch),
            None => text.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }
    if let Some((t, w)) = current.take() {
        push_word(&mut words, &mut text, t, w);
    }
    (text.trim().to_string(), words)
}

fn push_word(words: &mut Vec<LyricWord>, text: &mut String, t: u64, w: String) {
    let trimmed = w.trim();
    if trimmed.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with(' ') {
        text.push(' ');
    }
    text.push_str(trimmed);
    words.push(LyricWord {
        time_ms: t,
        text: trimmed.to_string(),
    });
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, `hh:mm:ss.xx`
fn parse_timestamp(tag: &str) -> Option<u64> {
    let parts: Vec<&str> = tag.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut ms: u64 = 0;
    for (i, p) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        if is_last {
            let (secs, frac) = p.split_once(['.', ',']).unwrap_or((p, ""));
            let secs: u64 = secs.parse().ok()?;
            let frac_ms = match frac.len() {
                0 => 0,
                1 => frac.parse::<u64>().ok()? * 100,
                2 => frac.parse::<u64>().ok()? * 10,
                _ => frac[..3].parse::<u64>().ok()?,
            };
            ms = ms * 60 + secs;
            ms = ms * 1000 + frac_ms;
        } else {
            let v: u64 = p.parse().ok()?;
            ms = ms * 60 + v;
        }
    }
    Some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_skips_metadata() {
        let lrc = "[ar:Someone]\n[00:12.50]First line\n[00:15.2]Second\n\n[01:02.345]Third";
        let l = parse_lrc(lrc);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].time_ms, 12_500);
        assert_eq!(l[0].text, "First line");
        assert!(l[0].words.is_empty());
        assert_eq!(l[1].time_ms, 15_200);
        assert_eq!(l[2].time_ms, 62_345);
    }

    #[test]
    fn multiple_timestamps_per_line() {
        let l = parse_lrc("[00:05.00][00:20.00]Chorus");
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].time_ms, 20_000);
        assert_eq!(l[1].text, "Chorus");
    }

    #[test]
    fn keeps_empty_lines_as_gaps() {
        let l = parse_lrc("[00:01.00]\n[00:02.00]x");
        assert_eq!(l[0].text, "");
    }

    #[test]
    fn enhanced_word_tags() {
        let l = parse_lrc("[00:10.00]<00:10.00>Hello <00:10.50>there, <00:11.20>world");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].text, "Hello there, world");
        assert_eq!(
            l[0].words,
            vec![
                LyricWord {
                    time_ms: 10_000,
                    text: "Hello".into()
                },
                LyricWord {
                    time_ms: 10_500,
                    text: "there,".into()
                },
                LyricWord {
                    time_ms: 11_200,
                    text: "world".into()
                },
            ]
        );
    }
}
