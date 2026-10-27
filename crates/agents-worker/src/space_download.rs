//! Conditional reads use immutable content identity only after owner/lifecycle
//! lookup. Partial reads stay native R2 streams rather than sliced full downloads.

use worker::{Env, Request, Response, Result};

use super::{OwnedFile, content_disposition, fail, file_object_range, get_owned_file, not_found};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    offset: u64,
    length: u64,
}

impl ByteRange {
    fn header(self, size: u64) -> String {
        format!(
            "bytes {}-{}/{size}",
            self.offset,
            self.offset + self.length - 1
        )
    }

    fn native(self) -> worker::Range {
        worker::Range::OffsetWithLength {
            offset: self.offset,
            length: self.length,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Read {
    Full,
    Partial(ByteRange),
    NotModified,
    Unsatisfiable,
}

pub(super) async fn content(req: &Request, env: &Env, owner: &str, id: &str) -> Result<Response> {
    let Some(file) = get_owned_file(env, owner, id).await? else {
        return not_found();
    };
    let etag = format!("\"{}\"", file.sha256);
    let read = plan(
        req.headers().get("If-None-Match")?.as_deref(),
        req.headers().get("Range")?.as_deref(),
        req.headers().get("If-Range")?.as_deref(),
        &etag,
        file.bytes,
    );
    let mut response = match read {
        Read::NotModified => Response::empty()?.with_status(304),
        Read::Unsatisfiable => {
            let mut response = crate::util::json_error(
                416,
                "invalid_range",
                "Requested file byte range cannot be satisfied",
            )?;
            response
                .headers_mut()
                .set("Content-Range", &format!("bytes */{}", file.bytes))?;
            response
        }
        Read::Full => body(env, owner, &file, None).await?,
        Read::Partial(range) => body(env, owner, &file, Some(range)).await?,
    };
    let headers = response.headers_mut();
    headers.set("ETag", &etag)?;
    headers.set("Accept-Ranges", "bytes")?;
    headers.set("X-Content-Sha256", &file.sha256)?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    Ok(response)
}

async fn body(
    env: &Env,
    owner: &str,
    file: &OwnedFile,
    range: Option<ByteRange>,
) -> Result<Response> {
    let object = file_object_range(env, owner, file, range.map(ByteRange::native)).await?;
    let body = object.body().ok_or_else(|| fail("file_not_ready"))?;
    let mut response = Response::from_body(body.response_body()?)?;
    if let Some(range) = range {
        response = response.with_status(206);
        response
            .headers_mut()
            .set("Content-Range", &range.header(file.bytes))?;
    }
    let headers = response.headers_mut();
    headers.set("Content-Type", &file.content_type)?;
    headers.set(
        "Content-Length",
        &range.map_or(file.bytes, |range| range.length).to_string(),
    )?;
    headers.set("Content-Disposition", &content_disposition(&file.filename))?;
    Ok(response)
}

fn plan(
    if_none_match: Option<&str>,
    range: Option<&str>,
    if_range: Option<&str>,
    etag: &str,
    size: u64,
) -> Read {
    if if_none_match.is_some_and(|value| matches_etag(value, etag)) {
        return Read::NotModified;
    }
    let Some(range) = range else {
        return Read::Full;
    };
    // A weak validator or unsupported date must not authorize assembling bytes
    // against a representation other than the caller's exact cached version.
    if if_range.is_some_and(|value| value.trim() != etag) {
        return Read::Full;
    }
    if let Some((unit, spec)) = range.trim().split_once('=') {
        // Unknown units must be ignored. Multiple ranges are valid HTTP but are
        // deliberately served whole rather than introducing multipart bodies.
        if !unit.eq_ignore_ascii_case("bytes") || spec.contains(',') {
            return Read::Full;
        }
    }
    match byte_range(range, size) {
        Some(range) => Read::Partial(range),
        None => Read::Unsatisfiable,
    }
}

fn matches_etag(value: &str, etag: &str) -> bool {
    if value.trim() == "*" {
        return true;
    }
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

fn number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn byte_range(value: &str, size: u64) -> Option<ByteRange> {
    if size == 0 {
        return None;
    }
    let (unit, spec) = value.trim().split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    if first.is_empty() {
        let length = number(last)?.min(size);
        return (length > 0).then_some(ByteRange {
            offset: size - length,
            length,
        });
    }
    let offset = number(first)?;
    if offset >= size {
        return None;
    }
    let end = if last.is_empty() {
        size - 1
    } else {
        number(last)?.min(size - 1)
    };
    if end < offset {
        return None;
    }
    Some(ByteRange {
        offset,
        length: end - offset + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn partial_reads_cover_closed_open_and_suffix_forms() {
        for (header, offset, length) in [
            ("bytes=2-5", 2, 4),
            ("bytes=7-", 7, 3),
            ("bytes=-3", 7, 3),
            ("bytes=2-999", 2, 8),
            ("bytes=-999", 0, 10),
            ("BYTES=0002-0005", 2, 4),
        ] {
            let range = byte_range(header, 10).unwrap();
            assert_eq!(range, ByteRange { offset, length });
            assert_eq!(
                range.header(10),
                format!("bytes {offset}-{}/10", offset + length - 1)
            );
        }
    }

    #[test]
    fn invalid_or_unsatisfiable_ranges_never_select_a_body() {
        for header in [
            "",
            "bytes=",
            "bytes=-",
            "bytes=-0",
            "bytes=10-",
            "bytes=5-4",
            "bytes=+1-2",
            "bytes=1 -2",
            "bytes=0-18446744073709551616",
            "bytes=18446744073709551616-",
            "bytes=1-2-3",
        ] {
            assert_eq!(
                plan(None, Some(header), None, "\"sha\"", 10),
                Read::Unsatisfiable,
                "{header:?}"
            );
        }
        assert_eq!(byte_range("bytes=0-0", 0), None);
        assert_eq!(byte_range("bytes=-1", 0), None);
        for header in ["items=0-1", "bytes=0-1,4-5"] {
            assert_eq!(plan(None, Some(header), None, "\"sha\"", 10), Read::Full);
        }
    }

    #[test]
    fn cache_preconditions_precede_range_and_weak_tags_match_only_if_none_match() {
        for validator in ["\"sha\"", "W/\"sha\"", "*", "\"other\", W/\"sha\""] {
            assert_eq!(
                plan(Some(validator), Some("invalid"), None, "\"sha\"", 10),
                Read::NotModified
            );
        }
        assert_eq!(
            plan(
                Some("\"other\""),
                Some("bytes=1-2"),
                Some("\"sha\""),
                "\"sha\"",
                10
            ),
            Read::Partial(ByteRange {
                offset: 1,
                length: 2
            })
        );
        for validator in ["W/\"sha\"", "\"other\"", "Sun, 06 Nov 1994 08:49:37 GMT"] {
            assert_eq!(
                plan(None, Some("bytes=1-2"), Some(validator), "\"sha\"", 10),
                Read::Full
            );
        }
        assert_eq!(plan(None, None, None, "\"sha\"", 0), Read::Full);
    }

    proptest! {
        #[test]
        fn every_selected_range_is_nonempty_and_bounded(size in 0u64..67_108_865, first in 0u64..134_217_729, last in 0u64..134_217_729) {
            if let Some(range)=byte_range(&format!("bytes={first}-{last}"),size) {
                prop_assert!(range.length>0);
                prop_assert!(range.offset<size);
                prop_assert!(range.offset+range.length<=size);
            }
            if let Some(range)=byte_range(&format!("bytes=-{last}"),size) {
                prop_assert_eq!(range.length,last.min(size));
                prop_assert!(range.length>0);
                prop_assert_eq!(range.offset+range.length,size);
            }
        }
    }
}
