//! `http.fetch`: one request on ureq, to a host the manifest lists.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use ureq::http::{Method, Request, Uri, header};

use crate::host::HostError;
use crate::manifest::HostPattern;

/// A redirect is followed only to an allowed host, this many times.
const MAX_REDIRECTS: usize = 5;

/// What the script's `http.fetch` sends.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// The agent a plugin's requests share (its connection pool). Redirects are
/// ours to follow, so each hop is checked; a 4xx or 5xx is a response, not an
/// error.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent("diffui")
        .build()
        .new_agent()
}

/// Parse `url` and refuse it unless it is http(s) to a listed host.
pub(crate) fn check(url: &str, allowed: &[HostPattern]) -> Result<Uri, HostError> {
    let bad = |why: &str| HostError::new("TypeError", format!("http.fetch: \"{url}\" {why}"));
    let uri: Uri = url.parse().map_err(|_| bad("is not a URL"))?;
    let default_port = match uri.scheme_str() {
        Some("https") => 443,
        Some("http") => 80,
        _ => return Err(bad("must be an http or https URL")),
    };
    let authority = uri.authority().ok_or_else(|| bad("has no host"))?;
    if authority.as_str().contains('@') {
        return Err(bad("must not carry a user or password"));
    }
    let port = uri.port_u16().unwrap_or(default_port);
    if allowed
        .iter()
        .any(|pattern| pattern.matches(authority.host(), port))
    {
        return Ok(uri);
    }
    let listed = match allowed {
        [] => "it lists none".to_string(),
        hosts => format!(
            "it allows {}",
            hosts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Err(HostError::denied(format!(
        "http.fetch to {authority} is not allowed: plugin.toml's [permissions] http must list the host, and {listed}"
    )))
}

/// Send the request and read the whole response, following redirects that
/// stay on allowed hosts.
pub(crate) fn fetch(
    agent: &ureq::Agent,
    request: FetchRequest,
    allowed: &[HostPattern],
    default_timeout: Duration,
    max_body: u64,
) -> Result<Value, HostError> {
    let mut uri = check(&request.url, allowed)?;
    let mut method = match request.method.as_str() {
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" => {
            Method::from_bytes(request.method.as_bytes()).expect("a standard method")
        }
        other => {
            return Err(HostError::new(
                "TypeError",
                format!("http.fetch: \"{other}\" is not a method"),
            ));
        }
    };
    let mut headers = request.headers;
    let mut body = request.body;
    let timeout = request
        .timeout_ms
        .map_or(default_timeout, Duration::from_millis);

    for _ in 0..=MAX_REDIRECTS {
        let mut builder = Request::builder().method(method.clone()).uri(uri.clone());
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        let invalid =
            |error: ureq::http::Error| HostError::new("TypeError", format!("http.fetch: {error}"));
        let sent = match &body {
            Some(text) => {
                let request = builder.body(text.as_str()).map_err(invalid)?;
                agent.run(
                    agent
                        .configure_request(request)
                        .timeout_global(Some(timeout))
                        .build(),
                )
            }
            None => {
                let request = builder.body(()).map_err(invalid)?;
                agent.run(
                    agent
                        .configure_request(request)
                        .timeout_global(Some(timeout))
                        .build(),
                )
            }
        };
        let mut response = sent.map_err(|error| transport(&uri, error))?;
        let status = response.status();

        let location = response
            .headers()
            .get(header::LOCATION)
            .filter(|_| status.is_redirection())
            .and_then(|value| value.to_str().ok());
        if let Some(location) = location {
            let next = redirect_target(&uri, location).ok_or_else(|| {
                HostError::new(
                    "NetworkError",
                    format!("http.fetch: {uri} redirected to \"{location}\", which is not a URL"),
                )
            })?;
            let next = check(&next, allowed).map_err(|error| HostError {
                message: format!("{} (redirected from {uri})", error.message),
                ..error
            })?;
            if next.authority() != uri.authority() {
                // A token for one host is not for the next.
                headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
            }
            // What browsers do: 303 always, 301 and 302 for a POST.
            if status == 303 || (matches!(status.as_u16(), 301 | 302) && method == Method::POST) {
                method = Method::GET;
                body = None;
                headers.retain(|(name, _)| !name.to_ascii_lowercase().starts_with("content-"));
            }
            uri = next;
            continue;
        }

        let text = response
            .body_mut()
            .with_config()
            .limit(max_body)
            .lossy_utf8(true)
            .read_to_string()
            .map_err(|error| transport(&uri, error))?;
        return Ok(json!({
            "status": status.as_u16(),
            "statusText": status.canonical_reason().unwrap_or(""),
            "url": uri.to_string(),
            "headers": response
                .headers()
                .iter()
                .map(|(name, value)| [name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()])
                .collect::<Vec<_>>(),
            "body": text,
        }));
    }
    Err(HostError::new(
        "NetworkError",
        format!(
            "http.fetch: {} redirected more than {MAX_REDIRECTS} times",
            request.url
        ),
    ))
}

/// Where a `Location` header points: absolute, or a path on the same origin.
fn redirect_target(from: &Uri, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        Some(location.to_string())
    } else if location.starts_with('/') && !location.starts_with("//") {
        Some(format!(
            "{}://{}{location}",
            from.scheme_str()?,
            from.authority()?
        ))
    } else {
        None
    }
}

fn transport(uri: &Uri, error: ureq::Error) -> HostError {
    let name = match error {
        ureq::Error::Timeout(_) => "TimeoutError",
        _ => "NetworkError",
    };
    HostError::new(name, format!("http.fetch {uri}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(list: &[&str]) -> Vec<HostPattern> {
        list.iter()
            .map(|host| HostPattern::parse(host).unwrap())
            .collect()
    }

    #[test]
    fn only_listed_hosts_over_http_pass() {
        let allowed = hosts(&["codeberg.org", "*.example.com", "127.0.0.1:3000"]);
        for url in [
            "https://codeberg.org/api/v1/user",
            "http://codeberg.org:8080/x?y=1",
            "https://git.example.com/",
            "http://127.0.0.1:3000/api",
        ] {
            assert!(check(url, &allowed).is_ok(), "{url}");
        }
        for (url, why) in [
            (
                "https://evil.test/",
                "http.fetch to evil.test is not allowed",
            ),
            ("https://codeberg.org.evil.test/", "is not allowed"),
            (
                "http://127.0.0.1:3001/",
                "http.fetch to 127.0.0.1:3001 is not allowed",
            ),
            (
                "https://codeberg.org@evil.test/",
                "must not carry a user or password",
            ),
            (
                "https://user:pw@codeberg.org/",
                "must not carry a user or password",
            ),
            ("file:///etc/passwd", "is not a URL"),
            ("ftp://codeberg.org/", "must be an http or https URL"),
            ("/relative", "must be an http or https URL"),
            ("not a url", "is not a URL"),
        ] {
            let error = check(url, &allowed).unwrap_err();
            assert!(error.message.contains(why), "{url}: {}", error.message);
        }
        let error = check("https://codeberg.org/", &[]).unwrap_err();
        assert_eq!(error.name, "PermissionError");
        assert!(
            error.message.ends_with("and it lists none"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_location_is_absolute_or_a_path_on_the_same_origin() {
        let from: Uri = "http://127.0.0.1:3000/a/b".parse().unwrap();
        assert_eq!(
            redirect_target(&from, "/c?d=1").as_deref(),
            Some("http://127.0.0.1:3000/c?d=1")
        );
        assert_eq!(
            redirect_target(&from, "https://other.test/x").as_deref(),
            Some("https://other.test/x")
        );
        assert_eq!(redirect_target(&from, "//other.test/x"), None);
        assert_eq!(redirect_target(&from, "c"), None);
    }
}
