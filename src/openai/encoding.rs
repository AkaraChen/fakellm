//! Request body `content-encoding`: zstd, gzip, deflate and br (pure Rust decoders), also stacked
//! (`gzip, zstd` was gzipped first). Clients such as codex compress their request bodies; the body is
//! decoded before it is parsed and logged. The decoded size is capped at `control::MAX_BODY`.

use std::io::Read;

use bytes::Bytes;

use crate::control::MAX_BODY;

/// Why a body could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// An encoding that is not supported (415).
    Unsupported(String),
    /// The decoded body is larger than `MAX_BODY` (413).
    TooLarge,
    /// The bytes are not valid for the declared encoding (400).
    Corrupt(&'static str),
}

/// The encodings named by the `content-encoding` headers, in the order they were applied. `identity` is dropped.
pub fn encodings(headers: &http::HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for v in headers.get_all(http::header::CONTENT_ENCODING) {
        for part in String::from_utf8_lossy(v.as_bytes()).split(',') {
            let part = part.trim().to_ascii_lowercase();
            if !part.is_empty() && part != "identity" {
                out.push(part);
            }
        }
    }
    out
}

/// Decodes `body` per `encodings`; `Ok(None)` when there is nothing to decode.
pub fn decode(encodings: &[String], body: &Bytes) -> Result<Option<Bytes>, Failure> {
    if encodings.is_empty() {
        return Ok(None);
    }
    for e in encodings {
        if !matches!(e.as_str(), "zstd" | "gzip" | "x-gzip" | "deflate" | "br") {
            return Err(Failure::Unsupported(e.clone()));
        }
    }
    let mut cur = body.to_vec();
    for e in encodings.iter().rev() {
        cur = decode_one(e, &cur)?;
    }
    Ok(Some(Bytes::from(cur)))
}

fn decode_one(encoding: &str, input: &[u8]) -> Result<Vec<u8>, Failure> {
    let limit = MAX_BODY as u64 + 1;
    let mut out = Vec::new();
    let read = match encoding {
        "zstd" => {
            let mut d = ruzstd::decoding::StreamingDecoder::new(input).map_err(|_| Failure::Corrupt("zstd"))?;
            (&mut d).take(limit).read_to_end(&mut out)
        }
        "gzip" | "x-gzip" => flate2::read::MultiGzDecoder::new(input).take(limit).read_to_end(&mut out),
        "deflate" => {
            // RFC 9110 says zlib-wrapped; some clients send a raw deflate stream.
            let zlib = flate2::read::ZlibDecoder::new(input).take(limit).read_to_end(&mut out);
            match zlib {
                Ok(n) => Ok(n),
                Err(_) if out.is_empty() => {
                    out.clear();
                    flate2::read::DeflateDecoder::new(input).take(limit).read_to_end(&mut out)
                }
                Err(e) => Err(e),
            }
        }
        _ => brotli::Decompressor::new(input, 4096).take(limit).read_to_end(&mut out),
    };
    if read.is_err() {
        return Err(Failure::Corrupt(match encoding {
            "zstd" => "zstd",
            "br" => "br",
            "deflate" => "deflate",
            _ => "gzip",
        }));
    }
    if out.len() > MAX_BODY {
        return Err(Failure::TooLarge);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn gz(b: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn stacked_and_identity() {
        let z = ruzstd::encoding::compress_to_vec(&gz(b"hello")[..], ruzstd::encoding::CompressionLevel::Fastest);
        let names = vec!["gzip".to_owned(), "zstd".to_owned()];
        assert_eq!(decode(&names, &Bytes::from(z)).unwrap().unwrap(), "hello");
        assert_eq!(decode(&[], &Bytes::new()).unwrap(), None);
    }

    #[test]
    fn rejects() {
        assert_eq!(
            decode(&["lzma".to_owned()], &Bytes::new()),
            Err(Failure::Unsupported("lzma".into()))
        );
        assert!(matches!(
            decode(&["gzip".to_owned()], &Bytes::from_static(b"nope")),
            Err(Failure::Corrupt(_))
        ));
        let big = gz(&vec![b'a'; MAX_BODY + 1]);
        assert_eq!(decode(&["gzip".to_owned()], &Bytes::from(big)), Err(Failure::TooLarge));
    }
}
