// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Summaries written by a service instead of by this machine.
//!
//! Speaks OpenAI's chat-completions shape. The article's full text goes to the
//! configured address. No streaming: the response has to parse as JSON.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::llm::engine::GenOptions;
use crate::shutdown::Cancel;

/// How long one summary may take. Generous: a large model on a busy service
/// can think for a while, and a timeout that fires early looks like a bug in
/// the program rather than a slow afternoon at the provider.
const TIMEOUT: Duration = Duration::from_secs(180);

/// A configured service. Cheap to build; the client inside pools connections,
/// so it is built once and kept.
pub struct Remote {
    client: reqwest::Client,
    url: String,
    key: Option<String>,
    model: String,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    max_tokens: i32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Content,
    /// Why the model stopped. "length" means it ran out of room, which is the
    /// difference between a service that failed and one that was not given
    /// enough space to answer.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Content {
    /// An `Option` because a reasoning model answers with `"content": null`
    /// and puts its thinking in another field. `#[serde(default)]` does not
    /// cover that — it fills in a missing field, not an explicit null — and
    /// without this the whole answer was refused as unreadable.
    #[serde(default)]
    content: Option<String>,
}

/// What a service says when it refuses. Every one of them answers an error with
/// JSON of this shape, and the message inside is the only useful part.
#[derive(Deserialize)]
struct Refusal {
    error: RefusalBody,
}

#[derive(Deserialize)]
struct RefusalBody {
    #[serde(default)]
    message: String,
    /// What the service in front passes on from the one behind it. A gateway
    /// answers "Provider returned error" and puts the actual complaint in here;
    /// without it the reader is told only that something went wrong somewhere.
    #[serde(default)]
    metadata: Option<RefusalDetail>,
}

#[derive(Deserialize)]
struct RefusalDetail {
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    provider_name: Option<String>,
}

impl Remote {
    /// Configure a service. `url` is the base or the endpoint; `model` is what
    /// the service calls it.
    pub fn new(url: &str, key: Option<&str>, model: &str) -> AppResult<Remote> {
        let url = url.trim().trim_end_matches('/');
        let model = model.trim();
        if url.is_empty() {
            return Err(AppError::told("api_no_url", "no address configured for the service"));
        }
        if model.is_empty() {
            return Err(AppError::told("api_no_model", "no model name configured for the service"));
        }

        // Half the world's documentation prints the base and the other half
        // prints the endpoint. Both are accepted rather than one being right.
        let url = match url.ends_with("/chat/completions") {
            true => url.to_string(),
            false => format!("{url}/chat/completions"),
        };

        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(crate::USER_AGENT)
            .build()
            .map_err(|e| AppError::Network(e.to_string()))?;

        Ok(Remote {
            client,
            url,
            key: key.map(str::trim).filter(|k| !k.is_empty()).map(str::to_string),
            model: model.to_string(),
        })
    }

    /// What the service is called, for the summary cache key: a summary written
    /// by one model must not be served as another's.
    pub fn id(&self) -> String {
        format!("api:{}", self.model)
    }

    /// Ask for one completion.
    ///
    /// Blocking, with the runtime built per call. Cancellation is checked before
    /// the request goes out, not during it.
    pub fn generate(
        &self,
        system: &str,
        user: &str,
        opts: &GenOptions,
        cancel: &Cancel,
    ) -> AppResult<String> {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }

        let body = Request {
            model: &self.model,
            messages: vec![
                Message { role: "system", content: system },
                Message { role: "user", content: user },
            ],
            temperature: opts.temp,
            max_tokens: opts.max_new_tokens,
            // The rest of GenOptions describes running a model in this process
            // — threads, context size, repetition penalties — and none of it is
            // ours to set on someone else's machine.
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AppError::Other(e.to_string()))?;

        runtime.block_on(async {
            let mut request = self.client.post(&self.url).json(&body);
            if let Some(key) = &self.key {
                request = request.bearer_auth(key);
            }

            let response = request
                .send()
                .await
                .map_err(|e| AppError::Network(format!("{}: {e}", self.url)))?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();

            if !status.is_success() {
                // What was sent, so a refusal that names no field can still be
                // traced to one. The two message bodies are cut to their first
                // eighty characters: their length is diagnostic, their contents
                // are an article and do not belong in a log.
                tracing::warn!("the request it refused: {}", outline(&body));
                return Err(refusal(status, &text));
            }

            let parsed: Response = serde_json::from_str(&text).map_err(|e| {
                AppError::told_about(
                    "api_unreadable",
                    format!("{}: {e}", self.url),
                    first_line(&text),
                )
            })?;

            let choice = parsed.choices.into_iter().next();
            let text = choice
                .as_ref()
                .and_then(|c| c.message.content.as_deref())
                .unwrap_or_default()
                .trim()
                .to_string();
            if !text.is_empty() {
                return Ok(text);
            }

            // Empty, and the reason matters: out of room is something the
            // reader can do something about, and everything else is not.
            let cut = choice.and_then(|c| c.finish_reason).as_deref() == Some("length");
            Err(match cut {
                true => AppError::told(
                    "api_truncated",
                    "the answer was cut off by the length limit before any text arrived",
                ),
                false => AppError::told("api_empty", "the service answered with no text"),
            })
        })
    }
}

/// Turn a refusal into something a reader can act on: the status separates a
/// bad key from an empty account from an unknown model.
fn refusal(status: reqwest::StatusCode, body: &str) -> AppError {
    // The whole body goes to the log: a gateway's own summary of a failure is
    // rarely the useful half, and the useful half is not always where this
    // expects it.
    tracing::warn!("service refused with HTTP {status}: {body}");

    let detail = match serde_json::from_str::<Refusal>(body) {
        Ok(r) => {
            let named = r.error.metadata.as_ref().and_then(|m| m.provider_name.clone());
            let upstream = r
                .error
                .metadata
                .as_ref()
                .and_then(|m| m.raw.clone())
                .map(|raw| first_line(&raw))
                .filter(|raw| !raw.trim().is_empty());
            match (named, upstream) {
                // "Provider returned error" on its own says nothing; with the
                // provider's own words after it, it says what to change.
                (Some(who), Some(what)) => format!("{} — {who}: {what}", r.error.message),
                (None, Some(what)) => format!("{} — {what}", r.error.message),
                _ => r.error.message,
            }
        }
        Err(_) => String::new(),
    };
    let detail = match detail.trim().is_empty() {
        true => first_line(body),
        false => detail.chars().take(300).collect(),
    };

    let code = match status.as_u16() {
        401 | 403 => "api_key_refused",
        404 => "api_no_such_model",
        429 => "api_too_many",
        402 => "api_out_of_credit",
        _ => "api_refused",
    };
    AppError::told_about(code, format!("HTTP {status}: {detail}"), detail)
}

/// The request as it went out, with the article cut down to a glimpse.
///
/// A service that answers "bad request" and names no field leaves only one way
/// to find out which field it meant: look at every field that was sent.
fn outline(request: &Request<'_>) -> String {
    let messages: Vec<String> = request
        .messages
        .iter()
        .map(|m| {
            let head: String = m.content.chars().take(80).collect();
            format!("{{role: {}, chars: {}, starts: {head:?}}}", m.role, m.content.chars().count())
        })
        .collect();
    format!(
        "model={:?} temperature={} max_tokens={} messages=[{}]",
        request.model,
        request.temperature,
        request.max_tokens,
        messages.join(", ")
    )
}

/// The first line of an answer, cut short: a service that fails with a page of
/// HTML should not put the page in a message box.
fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(200).collect()
}

/// How many tokens a service will see in `text`, near enough.
///
/// The tokenizer is on the far side of the wire, so this errs high. Counted per
/// script: Latin runs about four characters to a token, Cyrillic about two, CJK
/// about one.
pub fn token_estimate(text: &str) -> usize {
    let (mut latin, mut cyrillic, mut dense) = (0usize, 0usize, 0usize);
    for c in text.chars() {
        match c {
            '\u{4e00}'..='\u{9fff}' | '\u{3040}'..='\u{30ff}' | '\u{ac00}'..='\u{d7af}' => {
                dense += 1
            }
            '\u{0400}'..='\u{04ff}' => cyrillic += 1,
            _ => latin += 1,
        }
    }
    // Rounded up, so a short string never estimates as nothing.
    latin.div_ceil(3) + cyrillic.div_ceil(2) + dense
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_may_be_the_base_or_the_endpoint() {
        let base = Remote::new("https://api.openai.com/v1", None, "gpt-4o-mini").unwrap();
        assert_eq!(base.url, "https://api.openai.com/v1/chat/completions");

        let full = Remote::new("https://api.openai.com/v1/chat/completions/", None, "gpt-4o-mini")
            .unwrap();
        assert_eq!(full.url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn a_service_without_an_address_or_a_model_is_not_a_service() {
        assert!(Remote::new("  ", None, "gpt-4o-mini").is_err());
        assert!(Remote::new("https://example.com/v1", None, " ").is_err());
    }

    #[test]
    fn a_blank_key_is_no_key() {
        // A llama.cpp server on the next desk wants no Authorization header at
        // all, and an empty one is not the same as none.
        let none = Remote::new("http://localhost:8080/v1", Some("   "), "local").unwrap();
        assert!(none.key.is_none());
    }

    #[test]
    fn the_estimate_counts_each_script_at_its_own_rate() {
        // The same sentence in two alphabets does not cost the same, and an
        // average would be wrong for both.
        let english = token_estimate("the bank raised rates again this morning");
        let russian = token_estimate("банк снова поднял ставку сегодня утром");
        assert!(russian > english, "русский дороже: {russian} против {english}");

        // Never zero for something that is there, or the chunker would decide a
        // paragraph fits in nothing.
        assert!(token_estimate("a") >= 1);
        assert_eq!(token_estimate(""), 0);
    }

    /// Answers that must not be refused as unreadable.
    ///
    /// `"content": null` is the one that matters: a reasoning model answers
    /// that way, and `#[serde(default)]` does not cover an explicit null.
    #[test]
    fn the_shapes_a_service_really_answers_with() {
        let ok = r#"{"id":"x","provider":"AtlasCloud","choices":[{"message":{"content":"OK"}}]}"#;
        let parsed: Response = serde_json::from_str(ok).unwrap();
        assert_eq!(parsed.choices[0].message.content.as_deref(), Some("OK"));

        let null = r#"{"id":"x","choices":[{"message":{"content":null,"reasoning":"…"},
                       "finish_reason":"length"}]}"#;
        let parsed: Response = serde_json::from_str(null).unwrap();
        assert_eq!(parsed.choices[0].message.content, None);
        assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("length"));

        let nothing = r#"{"choices":[{"finish_reason":"stop"}]}"#;
        let parsed: Response = serde_json::from_str(nothing).unwrap();
        assert_eq!(parsed.choices[0].message.content, None);
    }

    /// A gateway's own words are the least useful half of a refusal.
    #[test]
    fn a_gateway_refusal_carries_what_the_provider_behind_it_said() {
        let body = r#"{"error":{"message":"Provider returned error","code":400,
                     "metadata":{"provider_name":"AtlasCloud",
                     "raw":"This model's maximum context length is 4096 tokens"}}}"#;
        let e = refusal(reqwest::StatusCode::BAD_REQUEST, body);
        let detail = e.detail().unwrap();
        assert!(detail.contains("AtlasCloud"), "{detail}");
        assert!(detail.contains("maximum context length"), "{detail}");
    }

    #[test]
    fn a_refusal_keeps_the_part_a_person_can_act_on() {
        let body = r#"{"error": {"message": "Incorrect API key provided: sk-xxx"}}"#;
        let e = refusal(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(e.code(), "api_key_refused");
        assert!(e.detail().unwrap().contains("Incorrect API key"));

        // A gateway that answers with HTML gets one line of it, not the page.
        let html = "<html>\n<head><title>502 Bad Gateway</title></head>\n</html>";
        let e = refusal(reqwest::StatusCode::BAD_GATEWAY, html);
        assert_eq!(e.code(), "api_refused");
        assert!(e.detail().unwrap().len() <= 200);
    }
}
