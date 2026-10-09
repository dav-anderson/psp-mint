use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub enum RpcError {
    Cookie(String),
    Http(reqwest::Error),
    Status(u16),
    Fault { code: i64, message: String },
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Cookie(why) => write!(f, "cannot read rpc cookie: {why}"),
            RpcError::Http(e) => write!(f, "rpc request failed: {e}"),
            RpcError::Status(code) => write!(f, "rpc returned http status {code}"),
            RpcError::Fault { code, message } => write!(f, "rpc error {code}: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

impl From<reqwest::Error> for RpcError {
    fn from(e: reqwest::Error) -> Self {
        RpcError::Http(e)
    }
}

#[derive(Deserialize)]
struct Fault {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct Reply {
    result: Option<Value>,
    error: Option<Fault>,
}

pub struct Rpc {
    url: String,
    user: String,
    pass: String,
    client: reqwest::blocking::Client,
}

pub fn parse_cookie(text: &str) -> Option<(&str, &str)> {
    text.trim().split_once(':')
}

impl Rpc {
    pub fn new(url: &str, user: &str, pass: &str) -> Result<Rpc, RpcError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Rpc {
            url: url.to_string(),
            user: user.to_string(),
            pass: pass.to_string(),
            client,
        })
    }

    pub fn from_cookie(url: &str, cookie_path: &Path) -> Result<Rpc, RpcError> {
        let text = fs::read_to_string(cookie_path)
            .map_err(|e| RpcError::Cookie(format!("{}: {e}", cookie_path.display())))?;
        let (user, pass) = parse_cookie(&text).ok_or_else(|| {
            RpcError::Cookie(format!("{}: not in user:password form", cookie_path.display()))
        })?;
        Rpc::new(url, user, pass)
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "psp-mint",
            "method": method,
            "params": params,
        });
        let resp = self
            .client
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&body)
            .send()?;
        let status = resp.status();
        let text = resp.text()?;
        let reply: Reply =
            serde_json::from_str(&text).map_err(|_| RpcError::Status(status.as_u16()))?;
        match reply.error {
            Some(Fault { code, message }) => Err(RpcError::Fault { code, message }),
            None => Ok(reply.result.unwrap_or(Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_with_trailing_newline() {
        assert_eq!(
            parse_cookie("__cookie__:abc123\n"),
            Some(("__cookie__", "abc123"))
        );
    }

    #[test]
    fn cookie_without_separator_is_rejected() {
        assert_eq!(parse_cookie("abc123"), None);
    }
}
