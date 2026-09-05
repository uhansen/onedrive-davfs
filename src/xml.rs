//! WebDAV XML helpers: `multistatus` response building plus the small
//! utilities (`http_date`, `xml_escape`, `pct_encode`) `PROPFIND` needs to
//! get exactly right, since `davfs2` is picky:
//!   - `getlastmodified` MUST be an IMF-fixdate (`"Mon, 12 Jan 2026 08:30:00
//!     GMT"`), not ISO 8601, or davfs2 treats the entry as perpetually stale
//!     and re-fetches it constantly.
//!   - `resourcetype` MUST be `<D:collection/>` for folders and empty for
//!     files, or davfs2 tries to `read()` a directory.
//!   - Every PROPFIND response is `207 Multi-Status`, never `200`.

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// One item to render as a `<D:response>` entry.
pub struct DavEntry {
    /// Path relative to the WebDAV root, already percent-decoded, no
    /// leading collection-vs-file distinction baked in (that's `is_dir`).
    pub href: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds.
    pub last_modified: u64,
    /// Opaque, quoted per RFC 7232 (e.g. Graph's `cTag`/`eTag`).
    pub etag: String,
}

/// Converts Unix seconds to RFC 7231 IMF-fixdate, e.g.
/// `"Mon, 12 Jan 2026 08:30:00 GMT"`. Implemented by hand (no `time` crate
/// dependency) using Howard Hinnant's days-from-civil algorithm, since pure
/// integer date math is simple, dependency-free, and trivially portable to
/// `wasm32-wasip2`.
pub fn http_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86400) as i64;
    let secs_of_day = unix_secs % 86400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );

    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[(((days % 7) + 11) % 7) as usize]; // 1970-01-01 was a Thursday (index 4)

    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        weekday,
        day,
        MONTHS[(month - 1) as usize],
        year,
        hour,
        minute,
        second
    )
}

/// Howard Hinnant's `civil_from_days`: converts a day count since the Unix
/// epoch (1970-01-01) into a `(year, month, day)` civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encodes a path segment for use in an `<D:href>`, leaving `/` as
/// a segment separator untouched.
pub fn pct_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn response_xml(entry: &DavEntry) -> String {
    let href = pct_encode(&entry.href);
    let href = if entry.is_dir && !href.ends_with('/') {
        format!("{href}/")
    } else {
        href
    };
    let resourcetype = if entry.is_dir { "<D:collection/>" } else { "" };
    let content_length = if entry.is_dir {
        String::new()
    } else {
        format!("<D:getcontentlength>{}</D:getcontentlength>", entry.size)
    };

    format!(
        r#"<D:response>
  <D:href>{href}</D:href>
  <D:propstat>
    <D:prop>
      <D:resourcetype>{resourcetype}</D:resourcetype>
      <D:getlastmodified>{last_modified}</D:getlastmodified>
      <D:getetag>&quot;{etag}&quot;</D:getetag>
      {content_length}
    </D:prop>
    <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
</D:response>"#,
        href = href,
        resourcetype = resourcetype,
        last_modified = http_date(entry.last_modified),
        etag = xml_escape(&entry.etag),
        content_length = content_length,
    )
}

/// Builds a full `207 Multi-Status` PROPFIND response body for one or more
/// entries (the resource itself, plus children when `Depth: 1`).
pub fn multistatus(entries: &[DavEntry]) -> String {
    let body: String = entries
        .iter()
        .map(response_xml)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
{body}
</D:multistatus>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_epoch() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn http_date_known_value() {
        // 2026-01-12T08:30:00Z
        let unix = 1768206600;
        assert_eq!(http_date(unix), "Mon, 12 Jan 2026 08:30:00 GMT");
    }

    #[test]
    fn xml_escape_special_chars() {
        assert_eq!(
            xml_escape(r#"a<b>c&d"e'f"#),
            "a&lt;b&gt;c&amp;d&quot;e&apos;f"
        );
    }

    #[test]
    fn pct_encode_preserves_slashes() {
        assert_eq!(pct_encode("/a b/c#d"), "/a%20b/c%23d");
    }

    #[test]
    fn multistatus_file_has_empty_resourcetype_and_length() {
        let entries = vec![DavEntry {
            href: "/report.docx".to_string(),
            is_dir: false,
            size: 42,
            last_modified: 0,
            etag: "abc123".to_string(),
        }];
        let xml = multistatus(&entries);
        assert!(xml.contains("207") == false); // status text lives in dav.rs, not here
        assert!(xml.contains("<D:resourcetype></D:resourcetype>"));
        assert!(xml.contains("<D:getcontentlength>42</D:getcontentlength>"));
        assert!(xml.contains("Thu, 01 Jan 1970 00:00:00 GMT"));
        assert!(!xml.ends_with('/'));
    }

    #[test]
    fn multistatus_folder_has_collection_resourcetype_and_trailing_slash_href() {
        let entries = vec![DavEntry {
            href: "/Documents".to_string(),
            is_dir: true,
            size: 0,
            last_modified: 0,
            etag: "xyz".to_string(),
        }];
        let xml = multistatus(&entries);
        assert!(xml.contains("<D:resourcetype><D:collection/></D:resourcetype>"));
        assert!(xml.contains("<D:href>/Documents/</D:href>"));
        assert!(!xml.contains("getcontentlength"));
    }

    #[test]
    fn multistatus_is_well_formed_multistatus_root() {
        let xml = multistatus(&[]);
        assert!(xml.contains("<D:multistatus xmlns:D=\"DAV:\">"));
        assert!(xml.trim_end().ends_with("</D:multistatus>"));
    }
}
