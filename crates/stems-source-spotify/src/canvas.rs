//! Spotify Canvas: the short looping clips shown behind tracks in the app.
//!
//! Uses the internal `canvaz-cache` endpoint through librespot's
//! authenticated spclient. The protobuf messages are tiny, so they are
//! encoded/decoded by hand here instead of depending on generated types
//! (librespot's bundled `canvaz.proto` is incomplete).
//!
//! Unofficial API: treat any failure as "no canvas".

use anyhow::{Context, Result, anyhow};
use librespot_core::{Session, SpotifyUri};

const ENDPOINT: &str = "/canvaz-cache/v0/canvases";

/// Canvaz.type values that are playable video loops.
const VIDEO_TYPES: [u64; 3] = [1, 2, 3];

/// Look up the canvas video URL for a track, if it has one.
pub async fn fetch_canvas_url(session: &Session, uri: &SpotifyUri) -> Result<Option<String>> {
    let uri_str = uri.to_uri().context("track uri")?;
    let body = encode_request(&uri_str);
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/x-protobuf"),
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/x-protobuf"),
    );
    let bytes = session
        .spclient()
        .request(&http::Method::POST, ENDPOINT, Some(headers), Some(&body))
        .await
        .map_err(|e| anyhow!("canvaz request: {e}"))?;
    Ok(decode_response(&bytes)?
        .into_iter()
        .find(|c| c.entity_uri == uri_str && VIDEO_TYPES.contains(&c.kind) && !c.url.is_empty())
        .map(|c| c.url))
}

#[derive(Debug, Default, PartialEq)]
struct Canvaz {
    url: String,
    kind: u64,
    entity_uri: String,
}

// --- minimal protobuf wire helpers -------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// `EntityCanvazRequest { entities: [ Entity { entity_uri } ] }`
fn encode_request(entity_uri: &str) -> Vec<u8> {
    let mut entity = Vec::new();
    put_bytes(&mut entity, 1, entity_uri.as_bytes());
    let mut req = Vec::new();
    put_bytes(&mut req, 1, &entity);
    req
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = *self
                .buf
                .get(self.pos)
                .ok_or_else(|| anyhow!("truncated varint"))?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(anyhow!("varint too long"));
            }
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).filter(|e| *e <= self.buf.len());
        let end = end.ok_or_else(|| anyhow!("truncated bytes"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Yields (field, wire type, payload-or-varint) until the end.
    fn next(&mut self) -> Result<Option<(u64, u8, Value<'a>)>> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = key >> 3;
        let wt = (key & 7) as u8;
        let val = match wt {
            0 => Value::Varint(self.varint()?),
            2 => Value::Bytes(self.bytes()?),
            1 => {
                self.pos += 8;
                Value::Skip
            }
            5 => {
                self.pos += 4;
                Value::Skip
            }
            other => return Err(anyhow!("unsupported wire type {other}")),
        };
        Ok(Some((field, wt, val)))
    }
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Skip,
}

/// `EntityCanvazResponse { canvases: [Canvaz], ttl }` → the Canvaz list.
fn decode_response(buf: &[u8]) -> Result<Vec<Canvaz>> {
    let mut out = Vec::new();
    let mut r = Reader { buf, pos: 0 };
    while let Some((field, _, val)) = r.next()? {
        if field == 1 {
            if let Value::Bytes(b) = val {
                out.push(decode_canvaz(b)?);
            }
        }
    }
    Ok(out)
}

fn decode_canvaz(buf: &[u8]) -> Result<Canvaz> {
    let mut c = Canvaz::default();
    let mut r = Reader { buf, pos: 0 };
    while let Some((field, _, val)) = r.next()? {
        match (field, val) {
            (2, Value::Bytes(b)) => c.url = String::from_utf8_lossy(b).into_owned(),
            (4, Value::Varint(v)) => c.kind = v,
            (5, Value::Bytes(b)) => c.entity_uri = String::from_utf8_lossy(b).into_owned(),
            _ => {}
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_encoding_matches_wire_format() {
        let b = encode_request("spotify:track:abc");
        // field 1, len-delim, len 19 → inner: field 1, len-delim, len 17, bytes
        assert_eq!(&b[..2], &[0x0a, 19]);
        assert_eq!(&b[2..4], &[0x0a, 17]);
        assert_eq!(&b[4..], b"spotify:track:abc");
    }

    #[test]
    fn response_decoding_roundtrip() {
        let mut canvaz = Vec::new();
        put_bytes(&mut canvaz, 1, b"id1");
        put_bytes(&mut canvaz, 2, b"https://canvaz.scdn.co/x.mp4");
        put_varint(&mut canvaz, (4 << 3) | 0);
        put_varint(&mut canvaz, 2);
        put_bytes(&mut canvaz, 5, b"spotify:track:abc");
        let mut resp = Vec::new();
        put_bytes(&mut resp, 1, &canvaz);
        put_varint(&mut resp, (2 << 3) | 0);
        put_varint(&mut resp, 3600);

        let got = decode_response(&resp).unwrap();
        assert_eq!(
            got,
            vec![Canvaz {
                url: "https://canvaz.scdn.co/x.mp4".into(),
                kind: 2,
                entity_uri: "spotify:track:abc".into()
            }]
        );
    }
}
