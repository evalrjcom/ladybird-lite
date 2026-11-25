//! `RequestServer` stand-in: one blocking reqwest client with cookies, a browser
//! User-Agent, GET/POST and form submission encoding.

use crate::page::FormSpec;
use anyhow::{Context, Result};
use encoding_rs::{Encoding, UTF_8};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, REFERER};
use std::io::Read;
use std::time::Duration;
use url::Url;

/// Realistic browser UA (some sites, e.g. `DuckDuckGo`, reject unknown agents).
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0 MiniLadybird/0.2";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
// AUDIT FIX: separate, shorter connect timeout so dead hosts fail fast instead of after 20s.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// AUDIT FIX: explicit redirect cap (was implicit library default).
const MAX_REDIRECTS: usize = 10;
pub const MAX_BODY: usize = 8 * 1024 * 1024;

/// Encoded form data for a POST.
// AUDIT FIX: forms with enctype=multipart/form-data were silently sent urlencoded
// (servers expecting multipart reject them). Text fields are now sent as real multipart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormBody {
    UrlEncoded(String),
    Multipart(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// `Some(body)` => POST.
    pub form_body: Option<FormBody>,
    pub referer: Option<Url>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Request { url, form_body: None, referer: None }
    }
}

pub struct Response {
    pub url: Url,
    pub status: u16,
    pub body: String,
    // AUDIT FIX: expose the MIME type so non-HTML (images, binaries, text/plain) isn't parsed as HTML.
    pub mime: String,
    // AUDIT FIX: report truncation instead of silently cutting huge pages.
    pub truncated: bool,
    // AUDIT FIX: true when a redirect happened (a POST answered with 303 is now a GET document;
    // re-POSTing it on Back would be wrong).
    pub redirected: bool,
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .cookie_store(true)
        .build()?)
}

/// Referrer per the default `strict-origin-when-cross-origin`-ish policy:
/// no fragment/credentials, origin only across origins, nothing on HTTPS→HTTP downgrade.
// AUDIT FIX: the full URL (incl. #fragment, user:pass@, and query) was sent everywhere,
// even from https pages to http sites.
pub fn referer_for(from: &Url, to: &Url) -> Option<String> {
    if !matches!(from.scheme(), "http" | "https") {
        return None;
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return None;
    }
    let mut r = from.clone();
    r.set_fragment(None);
    let _ = r.set_username("");
    let _ = r.set_password(None);
    if r.origin() != to.origin() {
        r.set_path("/");
        r.set_query(None);
    }
    Some(r.to_string())
}

pub fn fetch(client: &Client, req: &Request) -> Result<Response> {
    let mut rb = match &req.form_body {
        Some(FormBody::UrlEncoded(body)) => client
            .post(req.url.as_str())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body.clone()),
        Some(FormBody::Multipart(pairs)) => {
            let form = pairs
                .iter()
                .fold(reqwest::blocking::multipart::Form::new(), |f, (k, v)| f.text(k.clone(), v.clone()));
            client.post(req.url.as_str()).multipart(form)
        }
        None => client.get(req.url.as_str()),
    };
    rb = rb.header(
        reqwest::header::ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    );
    if let Some(r) = req.referer.as_ref().and_then(|r| referer_for(r, &req.url)) {
        rb = rb.header(REFERER, r);
    }
    let resp = rb.send().context("Request failed")?;
    let url = resp.url().clone();
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // AUDIT FIX: the 8 MiB cap was applied *after* `resp.text()` had already buffered the whole
    // body (an endless/huge response could exhaust memory). Read at most MAX_BODY+1 bytes.
    let mut bytes = Vec::new();
    resp.take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .context("Failed to read body")?;
    let truncated = bytes.len() > MAX_BODY;
    bytes.truncate(MAX_BODY);
    let body = decode_body(&bytes, &content_type);
    Ok(Response {
        redirected: url != req.url,
        url,
        status,
        body,
        mime: mime_essence(&content_type),
        truncated,
    })
}

/// `text/html; charset=x` → `text/html`.
fn mime_essence(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn charset_param(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| Encoding::for_label(v.trim().trim_matches(['"', '\'']).as_bytes()))?
    })
}

/// Prescan the first 1024 bytes for `<meta charset=…>` / `<meta http-equiv … content="…charset=…">`.
fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(1024)];
    let lower: Vec<u8> = head.iter().map(u8::to_ascii_lowercase).collect();
    let mut from = 0;
    while let Some(pos) = find(&lower[from..], b"charset") {
        let mut i = from + pos + b"charset".len();
        from = i;
        while i < lower.len() && lower[i].is_ascii_whitespace() {
            i += 1;
        }
        if lower.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < lower.len() && (lower[i].is_ascii_whitespace() || lower[i] == b'"' || lower[i] == b'\'') {
            i += 1;
        }
        let start = i;
        while i < lower.len() && (lower[i].is_ascii_alphanumeric() || b"-_:.".contains(&lower[i])) {
            i += 1;
        }
        if let Some(enc) = Encoding::for_label(&lower[start..i]) {
            // HTML spec: a meta-declared UTF-16 means UTF-8 (the bytes are ASCII-compatible).
            return Some(if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE { UTF_8 } else { enc });
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode a body: BOM → Content-Type charset → `<meta>` prescan → UTF-8 (lossy).
// AUDIT FIX: `resp.text()` only honoured the HTTP header; pages declaring their charset via
// <meta> (common for windows-1252 / Shift_JIS sites) were shown as mojibake.
pub fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let enc = charset_param(content_type)
        .or_else(|| {
            let m = mime_essence(content_type);
            (m.is_empty() || m.contains("html")).then(|| sniff_meta_charset(bytes))?
        })
        .unwrap_or(UTF_8);
    // `decode` also performs BOM sniffing, which overrides the label (per the Encoding spec).
    let (text, _, _) = enc.decode(bytes);
    text.into_owned()
}

/// Build the HTTP request for submitting `form` with the given successful
/// name/value pairs (already in tree order), per the HTML form-submission algorithm.
/// `overrides` are the submitter's formaction/formmethod/formenctype, if any.
pub fn form_request(form: &FormSpec, pairs: &[(String, String)], referer: Option<Url>) -> Request {
    let mut url = form.action.clone();
    url.set_fragment(None);
    if form.post {
        let body = if form.multipart {
            FormBody::Multipart(pairs.to_vec())
        } else {
            FormBody::UrlEncoded(urlencode(pairs))
        };
        Request { url, form_body: Some(body), referer }
    } else {
        url.set_query(Some(&urlencode(pairs)));
        Request { url, form_body: None, referer }
    }
}

fn urlencode(pairs: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(String, String)> {
        vec![("q".into(), "rust fltk & co".into()), ("kl".into(), "us-en".into())]
    }

    fn form(action: &str, post: bool, multipart: bool) -> FormSpec {
        FormSpec { action: Url::parse(action).unwrap(), post, multipart, id: None, dialog: false }
    }

    #[test]
    fn get_replaces_query() {
        let r = form_request(&form("https://x.test/s?old=1#frag", false, false), &pairs(), None);
        assert_eq!(r.url.as_str(), "https://x.test/s?q=rust+fltk+%26+co&kl=us-en");
        assert!(r.form_body.is_none());
    }

    #[test]
    fn post_encodes_body() {
        let r = form_request(&form("https://lite.duckduckgo.com/lite/", true, false), &pairs(), None);
        assert_eq!(r.url.as_str(), "https://lite.duckduckgo.com/lite/");
        assert_eq!(r.form_body, Some(FormBody::UrlEncoded("q=rust+fltk+%26+co&kl=us-en".into())));
    }

    #[test]
    fn multipart_post() {
        let r = form_request(&form("https://x.test/up", true, true), &pairs(), None);
        assert_eq!(r.form_body, Some(FormBody::Multipart(pairs())));
    }

    #[test]
    fn referer_policy() {
        let from = Url::parse("https://u:p@a.test/path?q=1#frag").unwrap();
        let same = Url::parse("https://a.test/other").unwrap();
        let cross = Url::parse("https://b.test/").unwrap();
        let downgrade = Url::parse("http://a.test/").unwrap();
        assert_eq!(referer_for(&from, &same).as_deref(), Some("https://a.test/path?q=1"));
        assert_eq!(referer_for(&from, &cross).as_deref(), Some("https://a.test/"));
        assert_eq!(referer_for(&from, &downgrade), None);
    }

    #[test]
    fn charset_detection() {
        // windows-1252 "Café" declared only via <meta>
        let body = b"<html><head><meta charset=\"windows-1252\"></head><body>Caf\xe9</body></html>";
        assert!(decode_body(body, "text/html").contains("Café"));
        // header wins over meta
        let latin = b"<meta charset=utf-8>Caf\xe9";
        assert!(decode_body(latin, "text/html; charset=ISO-8859-1").contains("Café"));
        // http-equiv form
        let sjis = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\">\x93\xfa\x96\x7b";
        assert!(decode_body(sjis, "text/html").contains("日本"));
        // no declaration, invalid UTF-8 doesn't panic
        assert!(decode_body(b"ok \xff\xfe\xfd", "").starts_with("ok "));
        // UTF-8 BOM overrides a wrong label
        assert_eq!(decode_body(b"\xef\xbb\xbfh\xc3\xa9", "text/html; charset=windows-1252"), "hé");
    }

    #[test]
    fn mime() {
        assert_eq!(mime_essence("Text/HTML; charset=utf-8"), "text/html");
        assert_eq!(mime_essence(""), "");
    }
}
