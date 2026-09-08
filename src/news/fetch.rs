// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! HTTP: one shared client, an identifying User-Agent, a per-host delay, and a
//! cap on hosts in flight.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::error::{AppError, AppResult};
use crate::shutdown::Cancel;

/// Minimum gap between two requests to the same host.
const PER_HOST_DELAY: Duration = Duration::from_millis(700);

/// How many hosts may be in flight at once.
const MAX_CONCURRENT: usize = 6;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Fetcher {
    client: reqwest::Client,
    /// Last request to each host, so requests can be spaced out.
    last_hit: Mutex<HashMap<String, Instant>>,
    permits: Arc<Semaphore>,
}

/// What came back.
///
/// The final address, not the one asked for: publishers redirect, and the
/// address a page settles on is the one to remember it by.
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub final_url: String,
}

impl Fetcher {
    pub fn new() -> AppResult<Fetcher> {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            // Publishers redirect a lot (http→https, AMP, consent walls). Follow
            // a few, but not a loop.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        Ok(Fetcher {
            client,
            last_hit: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        })
    }

    /// GET `url`, honoring the politeness rules.
    ///
    /// No conditional request: every run asks the whole question and takes the
    /// whole answer, so the result depends on the window asked for rather than
    /// on when the program last ran.
    pub async fn get(&self, url: &str, cancel: &Cancel) -> AppResult<Fetched> {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }

        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::Other("fetch permit pool closed".into()))?;

        self.wait_turn(url).await;

        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("{url}: {e}")))?;

        if !response.status().is_success() {
            return Err(AppError::Network(format!("{url} returned HTTP {}", response.status())));
        }

        let final_url = response.url().to_string();
        let bytes = response.bytes().await.map_err(|e| AppError::Network(e.to_string()))?.to_vec();
        Ok(Fetched { bytes, final_url })
    }

    /// Convenience wrapper for endpoints with no caching story, like a JSON API.
    pub async fn get_text(&self, url: &str, cancel: &Cancel) -> AppResult<String> {
        self.get_text_with(url, &HashMap::new(), cancel).await
    }

    /// As `get_text`, plus headers the source supplies — an API token, usually.
    ///
    /// Header values are secrets: a malformed one is reported by name only, and
    /// nothing here writes a value anywhere.
    pub async fn get_text_with(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        cancel: &Cancel,
    ) -> AppResult<String> {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::Other("fetch permit pool closed".into()))?;
        self.wait_turn(url).await;

        let mut req = self.client.get(url);
        for (name, value) in headers {
            // Named refusals: these come from a line someone typed into the
            // source form, and they are read wherever that form is read.
            let header =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    AppError::told_about(
                        "bad_header_name",
                        format!("'{name}' is not a valid header name"),
                        name,
                    )
                })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                AppError::told_about(
                    "bad_header_value",
                    format!("the value of '{name}' is not valid in a header"),
                    name,
                )
            })?;
            req = req.header(header, value);
        }

        let response = req.send().await.map_err(|e| AppError::Network(format!("{url}: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            // A refusal should say what to do about it; the
            // status alone sends people looking through the code instead.
            return Err(AppError::Network(match status.as_u16() {
                401 | 403 => format!(
                    "{url} returned HTTP {status} — this service wants a token; add one under [source.headers]"
                ),
                429 => format!(
                    "{url} returned HTTP {status} — the service is rate-limiting us; collect less often"
                ),
                _ => format!("{url} returned HTTP {status}"),
            }));
        }
        let bytes = response.bytes().await.map_err(|e| AppError::Network(e.to_string()))?;
        Ok(decode_body(&bytes))
    }

    /// Sleep until this host's turn comes round.
    async fn wait_turn(&self, url: &str) {
        let host = match url::Url::parse(url) {
            Ok(u) => u.host_str().unwrap_or_default().to_string(),
            Err(_) => return,
        };

        let wait = {
            let mut map = self.last_hit.lock();
            let now = Instant::now();
            let wait = match map.get(&host) {
                Some(&last) => PER_HOST_DELAY.checked_sub(now.duration_since(last)),
                None => None,
            };
            // Claim the slot before releasing the lock, so two tasks racing on
            // the same host queue behind each other instead of both going now.
            map.insert(host, now + wait.unwrap_or_default());
            wait
        };

        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
    }
}

/// Decode a response body to text, letting the HTML's declared charset win.
///
/// Russian and German news sites still serve windows-1251 and iso-8859-1, which
/// a lossy UTF-8 read turns into replacement characters.
pub fn decode_body(bytes: &[u8]) -> String {
    // Look for a charset declaration in the first 2 KB, where <meta> lives.
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_lowercase();
    let declared = head
        .split("charset")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start_matches(['=', '"', '\'', ' '])
                .split(['"', '\'', ' ', '/', '>', ';'])
                .next()
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if let Some(label) = declared
        && let Some(encoding) = encoding_for(&label)
    {
        let (text, _, _) = encoding.decode(bytes);
        return text.into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn encoding_for(label: &str) -> Option<&'static encoding_rs::Encoding> {
    encoding_rs::Encoding::for_label(label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through() {
        assert_eq!(decode_body("привет".as_bytes()), "привет");
    }

    #[test]
    fn declared_windows_1251_is_honoured() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode("Новости дня");
        let mut page = b"<html><head><meta charset=\"windows-1251\"><title>".to_vec();
        page.extend_from_slice(&bytes);
        let text = decode_body(&page);
        assert!(text.contains("Новости дня"), "got: {text}");
    }

    #[test]
    fn unknown_charset_falls_back_to_utf8() {
        let page = "<meta charset=\"nonsense-42\">привет".as_bytes();
        assert!(decode_body(page).contains("привет"));
    }
}
