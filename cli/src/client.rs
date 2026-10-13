use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use std::path::Path;

/// Thin HTTP client over the ESPuino Label Printer REST API.
pub struct Client {
    base: String,
    http: reqwest::Client,
    /// Operator bearer token. `None` for pure customer-level use.
    token: Option<String>,
}

/// Guess a content type from a file extension. Matches the server's accepted
/// set; unknown extensions are sent as octet-stream and rejected server-side.
pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif")
    )
}

impl Client {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Result<Self> {
        let base = base.into();
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            token,
        })
    }

    async fn handle(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or(text);
            bail!("HTTP {status}: {msg}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("parsing response: {text}"))
    }

    /// Attach the operator bearer token when one is configured. Every request
    /// goes through `send`, so this is the only place that has to know: on a
    /// customer-level call the server just ignores the header.
    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = self
            .authorize(req)
            .send()
            .await
            .context("sending request")?;
        Self::handle(resp).await
    }

    pub async fn create_prefix(&self, uuid: Option<&str>) -> Result<Value> {
        let body = match uuid {
            Some(u) => serde_json::json!({ "uuid": u }),
            None => serde_json::json!({}),
        };
        self.send(
            self.http
                .post(format!("{}/api/prefixes", self.base))
                .json(&body),
        )
        .await
    }

    /// List prefixes, optionally filtered by status (`?status=`). Admin-only on
    /// the server, so this needs the operator token.
    pub async fn list_prefixes(&self, status: Option<&str>) -> Result<Value> {
        let mut req = self.http.get(format!("{}/api/prefixes", self.base));
        if let Some(s) = status {
            req = req.query(&[("status", s)]);
        }
        self.send(req).await
    }

    /// Set a prefix's status (any transition). Admin-only on the server.
    pub async fn set_status(
        &self,
        prefix: &str,
        status: &str,
        order_ref: Option<&str>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({ "status": status });
        if let Some(r) = order_ref {
            body["order_ref"] = serde_json::json!(r);
        }
        self.send(
            self.http
                .patch(format!("{}/api/prefixes/{prefix}/status", self.base))
                .json(&body),
        )
        .await
    }

    pub async fn get_prefix(&self, prefix: &str) -> Result<Value> {
        self.send(
            self.http
                .get(format!("{}/api/prefixes/{prefix}", self.base)),
        )
        .await
    }

    pub async fn list_cards(&self, prefix: &str) -> Result<Value> {
        self.send(
            self.http
                .get(format!("{}/api/prefixes/{prefix}/cards", self.base)),
        )
        .await
    }

    pub async fn get_card(&self, prefix: &str, id: i64) -> Result<Value> {
        self.send(
            self.http
                .get(format!("{}/api/prefixes/{prefix}/cards/{id}", self.base)),
        )
        .await
    }

    pub async fn delete_card(&self, prefix: &str, id: i64) -> Result<Value> {
        self.send(
            self.http
                .delete(format!("{}/api/prefixes/{prefix}/cards/{id}", self.base)),
        )
        .await
    }

    /// Upload one card image. `id = Some(_)` uses PUT (create/replace at that
    /// index); `None` lets the server assign the next id via POST.
    pub async fn upload_card(
        &self,
        prefix: &str,
        file: &Path,
        name: &str,
        serial_number: Option<&str>,
        back_text: Option<&str>,
        id: Option<i64>,
    ) -> Result<Value> {
        let bytes = tokio::fs::read(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let filename = file
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image")
            .to_string();

        let part = Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime_for(file))?;

        let mut form = Form::new()
            .text("name", name.to_string())
            .part("image", part);
        if let Some(s) = serial_number {
            form = form.text("serial_number", s.to_string());
        }
        if let Some(b) = back_text {
            form = form.text("back_text", b.to_string());
        }

        let req = match id {
            Some(id) => self
                .http
                .put(format!("{}/api/prefixes/{prefix}/cards/{id}", self.base)),
            None => self
                .http
                .post(format!("{}/api/prefixes/{prefix}/cards", self.base)),
        };
        self.send(req.multipart(form)).await
    }

    /// Upload (or replace) one card's colour back image, leaving its front
    /// alone. Only a `double_sided_color` prefix accepts this; the server
    /// answers 400 in the other two modes.
    pub async fn upload_back_image(&self, prefix: &str, id: i64, file: &Path) -> Result<Value> {
        let bytes = tokio::fs::read(file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let filename = file
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("back")
            .to_string();
        let part = Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime_for(file))?;
        let form = Form::new().part("back_image", part);
        self.send(
            self.http
                .put(format!(
                    "{}/api/prefixes/{prefix}/cards/{id}/back-image",
                    self.base
                ))
                .multipart(form),
        )
        .await
    }
}
