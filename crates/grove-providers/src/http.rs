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
        let text = resp.body_mut().read_to_string().map_err(net)?;
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, json))
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
    pub fn pages(&self, path: &str, per_page: usize, size_param: &str) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out = Vec::new();
        for page in 1..=200 {
            let v = self.ok(
                "GET",
                &format!("{path}{sep}{size_param}={per_page}&page={page}"),
                None,
            )?;
            let items = v.as_array().cloned().unwrap_or_default();
            let n = items.len();
            out.extend(items);
            if n < per_page {
                break;
            }
        }
        Ok(out)
    }
}
