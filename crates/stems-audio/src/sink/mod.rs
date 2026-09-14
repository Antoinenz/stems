//! Output sinks. Each sink owns a [`Consumer`](crate::Consumer) and drains it
//! on its own thread or device callback.

pub mod cpal;
pub mod null;
pub mod wav;

use std::path::PathBuf;
use std::str::FromStr;

/// Which sink to build, as parsed from the CLI / config:
/// `cpal`, `cpal:<device name>`, `wav:<path>`, `null`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkKind {
    Cpal { device: Option<String> },
    Wav { path: PathBuf },
    Null,
}

impl FromStr for SinkKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, arg) = match s.split_once(':') {
            Some((k, a)) => (k, Some(a)),
            None => (s, None),
        };
        match kind {
            "cpal" => Ok(SinkKind::Cpal {
                device: arg.filter(|a| !a.is_empty()).map(str::to_owned),
            }),
            "wav" => match arg {
                Some(p) if !p.is_empty() => Ok(SinkKind::Wav { path: p.into() }),
                _ => Err("wav sink needs a path: wav:<path>".into()),
            },
            "null" => Ok(SinkKind::Null),
            other => Err(format!(
                "unknown sink `{other}` (expected cpal, wav:<path>, null)"
            )),
        }
    }
}

impl std::fmt::Display for SinkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkKind::Cpal { device: None } => write!(f, "cpal"),
            SinkKind::Cpal { device: Some(d) } => write!(f, "cpal:{d}"),
            SinkKind::Wav { path } => write!(f, "wav:{}", path.display()),
            SinkKind::Null => write!(f, "null"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_roundtrip() {
        for s in ["null", "cpal", "cpal:HDMI 1", "wav:/tmp/o.wav"] {
            assert_eq!(s.parse::<SinkKind>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn parses_kinds() {
        assert_eq!("null".parse::<SinkKind>().unwrap(), SinkKind::Null);
        assert_eq!(
            "cpal".parse::<SinkKind>().unwrap(),
            SinkKind::Cpal { device: None }
        );
        assert_eq!(
            "cpal:HDMI 1".parse::<SinkKind>().unwrap(),
            SinkKind::Cpal {
                device: Some("HDMI 1".into())
            }
        );
        assert_eq!(
            "wav:/tmp/o.wav".parse::<SinkKind>().unwrap(),
            SinkKind::Wav {
                path: "/tmp/o.wav".into()
            }
        );
        assert!("wav".parse::<SinkKind>().is_err());
        assert!("bogus".parse::<SinkKind>().is_err());
    }
}
