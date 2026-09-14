//! Minimal LRC parser: `[mm:ss.xx]text`, multiple timestamps per line,
//! ignores metadata tags like `[ar:…]`.

use stems_core::LyricLine;

pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            match parse_timestamp(tag) {
                Some(ms) => times.push(ms),
                None => {
                    if times.is_empty() {
                        // metadata tag such as [ar:Artist]; skip the line
                        times.clear();
                        rest = "";
                        break;
                    }
                }
            }
            rest = &stripped[end + 1..];
        }
        let line = rest.trim();
        for ms in times {
            out.push(LyricLine {
                time_ms: ms,
                text: line.to_string(),
            });
        }
    }
    out.sort_by_key(|l| l.time_ms);
    out
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
        assert_eq!(
            l[0],
            LyricLine {
                time_ms: 12_500,
                text: "First line".into()
            }
        );
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
}
