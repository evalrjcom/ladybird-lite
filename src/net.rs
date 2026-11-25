//! RequestServer stand-in: one blocking reqwest client with cookies, a browser
//! User-Agent, GET/POST and form submission encoding.

use crate::page::FormSpec;
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, REFERER};
use std::time::Duration;
use url::Url;

/// Realistic browser UA (some sites, e.g. DuckDuckGo, reject unknown agents).
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0 MiniLadybird/0.2";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// `Some(body)` => POST application/x-www-form-urlencoded.
    pub form_body: Option<String>,
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
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .cookie_store(true)
        .build()?)
}

pub fn fetch(client: &Client, req: &Request) -> Result<Response> {
    let mut rb = match &req.form_body {
        Some(body) => client
            .post(req.url.as_str())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body.clone()),
        None => client.get(req.url.as_str()),
    };
    rb = rb.header(
        reqwest::header::ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    );
    if let Some(r) = &req.referer {
        rb = rb.header(REFERER, r.as_str());
    }
    let resp = rb.send().context("Request failed")?;
    let url = resp.url().clone();
    let status = resp.status().as_u16();
    let mut body = resp.text().context("Failed to read body")?;
    if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    Ok(Response { url, status, body })
}

/// Build the HTTP request for submitting `form` with the given successful
/// name/value pairs (already in tree order), per the HTML form-submission
/// algorithm for application/x-www-form-urlencoded.
pub fn form_request(form: &FormSpec, pairs: &[(String, String)], referer: Option<Url>) -> Request {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish();
    let mut url = form.action.clone();
    url.set_fragment(None);
    if form.post {
        Request { url, form_body: Some(encoded), referer }
    } else {
        url.set_query(Some(&encoded));
        Request { url, form_body: None, referer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(String, String)> {
        vec![("q".into(), "rust fltk & co".into()), ("kl".into(), "us-en".into())]
    }

    #[test]
    fn get_replaces_query() {
        let f = FormSpec { action: Url::parse("https://x.test/s?old=1#frag").unwrap(), post: false };
        let r = form_request(&f, &pairs(), None);
        assert_eq!(r.url.as_str(), "https://x.test/s?q=rust+fltk+%26+co&kl=us-en");
        assert!(r.form_body.is_none());
    }

    #[test]
    fn post_encodes_body() {
        let f = FormSpec { action: Url::parse("https://lite.duckduckgo.com/lite/").unwrap(), post: true };
        let r = form_request(&f, &pairs(), None);
        assert_eq!(r.url.as_str(), "https://lite.duckduckgo.com/lite/");
        assert_eq!(r.form_body.as_deref(), Some("q=rust+fltk+%26+co&kl=us-en"));
    }
}
