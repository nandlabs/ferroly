//! Request-body forms: `application/x-www-form-urlencoded` and
//! `multipart/form-data`, both parsed strictly and within explicit limits.

use ferroly::http::{percent_decode, HttpResponse, StatusCode};
use ferroly_derive::FerrolyError;

use super::respond::{IntoResponse, Problem};

/// Why a form body was refused.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[non_exhaustive]
pub enum FormError {
    /// The request's `Content-Type` is not the form type being read (or, for
    /// multipart, carries no usable boundary). Rendered as `415`.
    #[error("unsupported form content type: {0}")]
    ContentType(String),
    /// The body is not a well-formed form. Rendered as `400`.
    #[error("malformed form body: {0}")]
    Malformed(String),
    /// A configured limit was exceeded; the text names it. Rendered as `413`.
    #[error("form body exceeds a limit: {0}")]
    TooLarge(&'static str),
}

impl From<&FormError> for Problem {
    fn from(error: &FormError) -> Problem {
        let status = match error {
            FormError::ContentType(_) => StatusCode(415),
            FormError::Malformed(_) => StatusCode::BAD_REQUEST,
            FormError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        };
        Problem::new(status).detail(error.to_string())
    }
}

impl From<FormError> for Problem {
    fn from(error: FormError) -> Problem {
        Problem::from(&error)
    }
}

/// `415`, `400` or `413` as a problem document; see [`FormError`].
impl IntoResponse for FormError {
    fn into_response(self) -> HttpResponse {
        Problem::from(&self).into_response()
    }
}

/// Decodes one URL-encoded component: `+` becomes a space (when
/// `plus_as_space`), `%XX` escapes are decoded, and the result must be UTF-8.
/// `None` for a malformed escape or invalid UTF-8.
pub(crate) fn decode_component(input: &str, plus_as_space: bool) -> Option<String> {
    let bytes = if plus_as_space && input.contains('+') {
        percent_decode(&input.replace('+', " ")).ok()?
    } else {
        percent_decode(input).ok()?
    };
    String::from_utf8(bytes).ok()
}

/// Parses `key=value&…` pairs (a query string or a form body). Empty pairs
/// are skipped; a pair without `=` has an empty value.
pub(crate) fn parse_pairs(input: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((decode_component(key, true)?, decode_component(value, true)?));
    }
    Some(out)
}

/// The fields of a decoded `application/x-www-form-urlencoded` body, in the
/// order they were sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    pairs: Vec<(String, String)>,
}

impl Form {
    /// Parses a form body. Keys and values are percent-decoded (`+` is a
    /// space) and must be valid UTF-8.
    ///
    /// ```
    /// use ferroly::turbo::Form;
    ///
    /// let form = Form::parse(b"name=Ada+Lovelace&tag=a%26b&tag=c").unwrap();
    /// assert_eq!(form.get("name"), Some("Ada Lovelace"));
    /// assert_eq!(form.get_all("tag").collect::<Vec<_>>(), ["a&b", "c"]);
    /// assert!(Form::parse(b"broken=%zz").is_err());
    /// ```
    pub fn parse(body: &[u8]) -> Result<Form, FormError> {
        let text = std::str::from_utf8(body)
            .map_err(|_| FormError::Malformed("the body is not valid UTF-8".into()))?;
        let pairs = parse_pairs(text.trim_end_matches(['\r', '\n'])).ok_or_else(|| {
            FormError::Malformed("invalid percent-encoding or UTF-8 in a field".into())
        })?;
        Ok(Form { pairs })
    }

    /// The first value of the field called `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value of the field called `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// All `(name, value)` pairs, in order.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Consumes the form, returning the pairs.
    pub fn into_pairs(self) -> Vec<(String, String)> {
        self.pairs
    }

    /// The number of fields.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether the form has no fields.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// Limits applied while parsing a `multipart/form-data` body. Exceeding one
/// is [`FormError::TooLarge`] (`413`).
///
/// The whole request body is already bounded by the server's
/// [`ServerConfig::max_body_bytes`](ferroly::http::ServerConfig) (and any
/// per-route limit); these bound the pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartLimits {
    /// Maximum number of parts. Default 64.
    pub max_parts: usize,
    /// Maximum size of one part's content, in bytes. Default 8 MiB.
    pub max_part_bytes: usize,
    /// Maximum size of the whole body, in bytes. Default 16 MiB.
    pub max_total_bytes: usize,
    /// Maximum size of one part's header block, in bytes. Default 8 KiB.
    pub max_part_header_bytes: usize,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        MultipartLimits {
            max_parts: 64,
            max_part_bytes: 8 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
            max_part_header_bytes: 8 * 1024,
        }
    }
}

/// One part of a `multipart/form-data` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    data: Vec<u8>,
}

impl Part {
    /// The form field name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file name the client sent, exactly as sent. **Untrusted**: it may
    /// contain directory separators or `..`. Never join it onto a path — use
    /// [`safe_filename`](Self::safe_filename) or, better, a name you generate.
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// The last path component of the client's file name with control
    /// characters removed, or `None` when nothing usable is left (no file
    /// name, an empty one, `.` or `..`). Both `/` and `\` are treated as
    /// separators.
    pub fn safe_filename(&self) -> Option<String> {
        let raw = self.filename.as_deref()?;
        let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
        let clean: String = last.chars().filter(|c| !c.is_control()).collect();
        let clean = clean.trim().to_string();
        if clean.is_empty() || clean == "." || clean == ".." {
            None
        } else {
            Some(clean)
        }
    }

    /// The part's `Content-Type`, if it sent one.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// A header of this part (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The part's content.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The content as UTF-8 text, if it is valid UTF-8.
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }

    /// Consumes the part, returning its content.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Parses a `multipart/form-data` body (RFC 7578) within `limits`.
///
/// `content_type` is the request's `Content-Type` header, which carries the
/// boundary. Every part must have a `Content-Disposition: form-data` header
/// with a `name`.
///
/// ```
/// use ferroly::http::Multipart;
/// use ferroly::turbo::{parse_multipart, MultipartLimits};
///
/// let form = Multipart::new()
///     .text("title", "Q3")
///     .file("report", "q3.csv", "text/csv", b"a,b\n".to_vec());
/// let parts = parse_multipart(&form.content_type(), &form.encode(), &MultipartLimits::default())
///     .unwrap();
/// assert_eq!(parts[0].name(), "title");
/// assert_eq!(parts[0].text(), Some("Q3"));
/// assert_eq!(parts[1].filename(), Some("q3.csv"));
/// assert_eq!(parts[1].content_type(), Some("text/csv"));
/// assert_eq!(parts[1].data(), b"a,b\n");
/// ```
pub fn parse_multipart(
    content_type: &str,
    body: &[u8],
    limits: &MultipartLimits,
) -> Result<Vec<Part>, FormError> {
    let boundary = multipart_boundary(content_type)?;
    if body.len() > limits.max_total_bytes {
        return Err(FormError::TooLarge("total multipart size"));
    }
    let malformed = |what: &str| FormError::Malformed(what.to_string());

    let delimiter = format!("--{boundary}").into_bytes();
    let mut inner = b"\r\n".to_vec();
    inner.extend_from_slice(&delimiter);

    // The first delimiter is at the very start, or follows a preamble line.
    let mut pos = if body.starts_with(&delimiter) {
        delimiter.len()
    } else {
        match find(body, &inner, 0) {
            Some(at) => at + inner.len(),
            None => return Err(malformed("no boundary found")),
        }
    };

    let mut parts = Vec::new();
    loop {
        // After a delimiter: `--` closes the body, otherwise the line ends
        // (optional padding, then CRLF) and a part follows.
        if body[pos..].starts_with(b"--") {
            return Ok(parts);
        }
        while matches!(body.get(pos), Some(b' ' | b'\t')) {
            pos += 1;
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err(malformed("boundary line is not terminated by CRLF"));
        }
        pos += 2;
        if parts.len() >= limits.max_parts {
            return Err(FormError::TooLarge("number of parts"));
        }

        // Part headers, up to the blank line.
        let head_end = if body[pos..].starts_with(b"\r\n") {
            pos
        } else {
            let window_end = body.len().min(
                pos.saturating_add(limits.max_part_header_bytes)
                    .saturating_add(4),
            );
            match find(&body[..window_end], b"\r\n\r\n", pos) {
                Some(at) => at + 2,
                None if window_end < body.len() => {
                    return Err(FormError::TooLarge("part header size"))
                }
                None => return Err(malformed("part headers are not terminated")),
            }
        };
        if head_end - pos > limits.max_part_header_bytes {
            return Err(FormError::TooLarge("part header size"));
        }
        let headers = parse_part_headers(&body[pos..head_end])?;
        let data_start = head_end + 2;

        // Part content, up to the next delimiter.
        let data_end = find(body, &inner, data_start)
            .ok_or_else(|| malformed("closing boundary is missing"))?;
        if data_end - data_start > limits.max_part_bytes {
            return Err(FormError::TooLarge("part size"));
        }
        pos = data_end + inner.len();

        let disposition = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| malformed("a part has no Content-Disposition"))?;
        let (kind, params) = parse_header_params(disposition);
        if !kind.eq_ignore_ascii_case("form-data") {
            return Err(malformed("a part's Content-Disposition is not form-data"));
        }
        let name = params
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| malformed("a part has no name"))?;
        let filename = params
            .iter()
            .find(|(k, _)| k == "filename")
            .map(|(_, v)| v.clone());
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        parts.push(Part {
            name,
            filename,
            content_type,
            headers,
            data: body[data_start..data_end].to_vec(),
        });
    }
}

/// Extracts and validates the boundary of a `multipart/form-data` content
/// type.
fn multipart_boundary(content_type: &str) -> Result<String, FormError> {
    let (media, params) = parse_header_params(content_type);
    if !media.eq_ignore_ascii_case("multipart/form-data") {
        return Err(FormError::ContentType(media));
    }
    let boundary = params
        .into_iter()
        .find(|(k, _)| k == "boundary")
        .map(|(_, v)| v)
        .ok_or_else(|| FormError::ContentType("multipart/form-data without a boundary".into()))?;
    // RFC 2046: 1 to 70 characters from a restricted set, not ending in a space.
    let valid = !boundary.is_empty()
        && boundary.len() <= 70
        && !boundary.ends_with(' ')
        && boundary
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" '()+_,-./:=?".contains(&b));
    if !valid {
        return Err(FormError::ContentType(
            "multipart/form-data with an invalid boundary".into(),
        ));
    }
    Ok(boundary)
}

fn parse_part_headers(block: &[u8]) -> Result<Vec<(String, String)>, FormError> {
    let malformed = |what: &str| FormError::Malformed(what.to_string());
    let text =
        std::str::from_utf8(block).map_err(|_| malformed("part headers are not valid UTF-8"))?;
    let mut headers = Vec::new();
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed("a part header line has no colon"))?;
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b >= 0x7f) {
            return Err(malformed("a part header name is invalid"));
        }
        if value.bytes().any(|b| b < b' ' && b != b'\t') {
            return Err(malformed("a part header value has a control character"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

/// Splits `value; k=v; k2="quoted"` into the leading value and its
/// parameters. Parameter names are lower-cased; quoted values honour
/// backslash escapes, and the `%22` / `%0D` / `%0A` escapes that
/// `multipart/form-data` uses for `"`, CR and LF are decoded.
pub(crate) fn parse_header_params(input: &str) -> (String, Vec<(String, String)>) {
    let bytes = input.as_bytes();
    let first_end = input.find(';').unwrap_or(input.len());
    let first = input[..first_end].trim().to_string();
    let mut params = Vec::new();
    let mut i = first_end;
    while i < bytes.len() {
        // Skip the separator and any whitespace.
        while i < bytes.len() && (bytes[i] == b';' || bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b';' {
            i += 1;
        }
        let name = input[name_start..i].trim().to_ascii_lowercase();
        if i >= bytes.len() || bytes[i] == b';' {
            continue; // a flag without a value: ignored
        }
        i += 1; // '='
        let mut value = Vec::new();
        if bytes.get(i) == Some(&b'"') {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                }
                value.push(bytes[i]);
                i += 1;
            }
            i += 1; // closing quote
        } else {
            while i < bytes.len() && bytes[i] != b';' {
                value.push(bytes[i]);
                i += 1;
            }
            while matches!(value.last(), Some(b' ' | b'\t')) {
                value.pop();
            }
        }
        let value = String::from_utf8_lossy(&value)
            .replace("%22", "\"")
            .replace("%0D", "\r")
            .replace("%0d", "\r")
            .replace("%0A", "\n")
            .replace("%0a", "\n");
        if !name.is_empty() {
            params.push((name, value));
        }
    }
    (first, params)
}

/// The first position at or after `from` where `needle` occurs.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() || haystack.len() - from < needle.len() {
        return None;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        match haystack[i..=last_start].iter().position(|&b| b == first) {
            Some(offset) => {
                i += offset;
                if haystack[i..i + needle.len()] == *needle {
                    return Some(i);
                }
                i += 1;
            }
            None => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "multipart/form-data; boundary=XyZ";

    fn body(parts: &[&str]) -> Vec<u8> {
        let mut out = String::new();
        for part in parts {
            out.push_str("--XyZ\r\n");
            out.push_str(part);
            out.push_str("\r\n");
        }
        out.push_str("--XyZ--\r\n");
        out.into_bytes()
    }

    #[test]
    fn decodes_components() {
        assert_eq!(decode_component("a%20b+c", true).as_deref(), Some("a b c"));
        assert_eq!(decode_component("a+b%2B", false).as_deref(), Some("a+b+"));
        assert_eq!(decode_component("%ff", true), None);
        assert_eq!(decode_component("100%", true), None);
    }

    #[test]
    fn parses_pairs() {
        let pairs = parse_pairs("a=1&&b&c=x%3Dy").unwrap();
        assert_eq!(
            pairs,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), String::new()),
                ("c".to_string(), "x=y".to_string()),
            ]
        );
        assert!(parse_pairs("a=%zz").is_none());
    }

    #[test]
    fn parses_multipart_parts() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"a\"\r\n\r\none",
            "Content-Disposition: form-data; name=\"f\"; filename=\"../x%22y.txt\"\r\nContent-Type: text/plain\r\n\r\nline1\r\nline2",
        ]);
        let parts = parse_multipart(CT, &raw, &MultipartLimits::default()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name(), "a");
        assert_eq!(parts[0].data(), b"one");
        assert_eq!(parts[1].filename(), Some("../x\"y.txt"));
        assert_eq!(parts[1].safe_filename().as_deref(), Some("x\"y.txt"));
        assert_eq!(parts[1].data(), b"line1\r\nline2");
        assert_eq!(parts[1].header("content-type"), Some("text/plain"));
    }

    #[test]
    fn multipart_limits_are_enforced() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"a\"\r\n\r\n12345",
            "Content-Disposition: form-data; name=\"b\"\r\n\r\n1",
        ]);
        let limits = |f: fn(&mut MultipartLimits)| {
            let mut l = MultipartLimits::default();
            f(&mut l);
            l
        };
        assert_eq!(
            parse_multipart(CT, &raw, &limits(|l| l.max_parts = 1)),
            Err(FormError::TooLarge("number of parts"))
        );
        assert_eq!(
            parse_multipart(CT, &raw, &limits(|l| l.max_part_bytes = 4)),
            Err(FormError::TooLarge("part size"))
        );
        assert_eq!(
            parse_multipart(CT, &raw, &limits(|l| l.max_total_bytes = 10)),
            Err(FormError::TooLarge("total multipart size"))
        );
        assert_eq!(
            parse_multipart(CT, &raw, &limits(|l| l.max_part_header_bytes = 8)),
            Err(FormError::TooLarge("part header size"))
        );
    }

    #[test]
    fn multipart_rejects_malformed_input() {
        let limits = MultipartLimits::default();
        assert!(matches!(
            parse_multipart("text/plain", b"", &limits),
            Err(FormError::ContentType(_))
        ));
        assert!(matches!(
            parse_multipart("multipart/form-data", b"", &limits),
            Err(FormError::ContentType(_))
        ));
        assert!(matches!(
            parse_multipart(CT, b"no boundary here", &limits),
            Err(FormError::Malformed(_))
        ));
        // Missing closing boundary.
        let raw = b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\none".to_vec();
        assert!(matches!(
            parse_multipart(CT, &raw, &limits),
            Err(FormError::Malformed(_))
        ));
        // A part without a name.
        let raw = body(&["Content-Disposition: form-data\r\n\r\none"]);
        assert!(matches!(
            parse_multipart(CT, &raw, &limits),
            Err(FormError::Malformed(_))
        ));
    }

    #[test]
    fn safe_filename_strips_directories() {
        let part = |name: &str| Part {
            name: "f".into(),
            filename: Some(name.into()),
            content_type: None,
            headers: Vec::new(),
            data: Vec::new(),
        };
        assert_eq!(part("a.txt").safe_filename().as_deref(), Some("a.txt"));
        assert_eq!(
            part("../../etc/passwd").safe_filename().as_deref(),
            Some("passwd")
        );
        assert_eq!(
            part("C:\\dir\\b.txt").safe_filename().as_deref(),
            Some("b.txt")
        );
        assert_eq!(part("..").safe_filename(), None);
        assert_eq!(part("dir/").safe_filename(), None);
    }
}
