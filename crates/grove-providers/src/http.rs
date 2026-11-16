use crate::{Error, Result};
use serde_json::Value;
use std::time::Duration;

pub struct Http {
    base: String,
    auth: String,
    agent: ureq::Agent,
}

impl Http {
    pub fn new(base: &str, auth: String) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Http {
            base: base.trim_end_matches('/').to_string(),
            auth,
            agent,
        }
    }

    /// Returns the status and the parsed JSON body (`Null` when empty or not JSON).
    pub fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        self.send_h(method, path, body)
            .map(|(status, json, _)| (status, json))
    }

    /// Like [`send`], also returning the pagination hints of the response headers.
    fn send_h(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value, PageHints)> {
        let url = format!("{}{}", self.base, path);
        let net = |e: ureq::Error| Error::Network(e.to_string());
        let mut resp = match (method, body) {
            ("GET", _) => self
                .agent
                .get(&url)
                .header("Authorization", &self.auth)
                .header("Accept", "application/json")
                .call(),
            ("POST", Some(b)) => self
                .agent
                .post(&url)
                .header("Authorization", &self.auth)
                .header("Accept", "application/json")
                .send_json(b),
            _ => return Err(Error::Other(format!("unsupported request {method}"))),
        }
        .map_err(net)?;
        let status = resp.status().as_u16();
        let header = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let hints = PageHints {
            total: header("x-total-count").and_then(|v| v.trim().parse().ok()),
            link: header("link"),
        };
        let text = resp.body_mut().read_to_string().map_err(net)?;
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, json, hints))
    }

    /// Like [`send`], but maps non-2xx statuses to [`Error::Api`].
    pub fn ok(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let (status, json) = self.send(method, path, body)?;
        if (200..300).contains(&status) {
            return Ok(json);
        }
        let message = json["message"]
            .as_str()
            .unwrap_or("(no message)")
            .to_string();
        Err(Error::Api { status, message })
    }

    /// Every item of a paginated list endpoint (`path` may already carry a query string).
    ///
    /// A short page is not the end (a server may cap the page size below what was asked). It
    /// ends on `X-Total-Count` being reached, on a `Link` header without `rel="next"` (also when
    /// absent, for `link_style` servers such as GitHub), or on an empty or repeated page.
    pub fn pages(
        &self,
        path: &str,
        per_page: usize,
        size_param: &str,
        link_style: bool,
    ) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out: Vec<Value> = Vec::new();
        let mut prev: Option<Vec<Value>> = None;
        for page in 1..=500 {
            let url = format!("{path}{sep}{size_param}={per_page}&page={page}");
            let (status, v, hints) = self.send_h("GET", &url, None)?;
            if !(200..300).contains(&status) {
                let message = v["message"].as_str().unwrap_or("(no message)").to_string();
                return Err(Error::Api { status, message });
            }
            let items = v.as_array().cloned().unwrap_or_default();
            if items.is_empty() || prev.as_ref() == Some(&items) {
                break;
            }
            out.extend(items.iter().cloned());
            // GitHub style: no `Link: rel="next"` means this was the last page.
            let no_next = hints
                .link
                .as_deref()
                .map_or(link_style, |l| !l.contains("rel=\"next\""));
            if hints.total.is_some_and(|t| out.len() >= t) || no_next {
                break;
            }
            prev = Some(items);
        }
        Ok(out)
    }
}

/// What the response headers say about the rest of a paginated list.
struct PageHints {
    total: Option<usize>,
    link: Option<String>,
}
