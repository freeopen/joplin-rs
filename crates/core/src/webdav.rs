//! Minimal WebDAV client for read-only sync (PROPFIND/GET + PUT/MKCOL/DELETE
//! for diagnostics). Mirrors the behaviour Joplin's WebDavApi relies on:
//! Basic auth, 404 => null stat, multistatus parsing tolerant of namespace
//! prefix variants (d:, D:, none).

use crate::error::{Error, Result};
use base64::Engine;
use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

#[derive(Debug, Clone)]
pub struct WebDavClient {
    base_url: Url,
    auth_header: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStat {
    pub path: String,
    pub is_dir: bool,
    /// Last-modified in ms since epoch (0 when missing)
    pub updated_time: i64,
}

impl WebDavClient {
    pub fn new(base_url: &str, username: &str, password: &str) -> Result<Self> {
        let mut url = Url::parse(base_url).map_err(|e| Error::WebDav(format!("bad base url: {e}")))?;
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let auth_header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        );
        let http = reqwest::Client::builder()
            .user_agent("joplin-rs/0.1")
            .build()
            .map_err(|e| Error::WebDav(format!("http client: {e}")))?;
        Ok(Self { base_url: url, auth_header, http })
    }

    fn url_for(&self, path: &str) -> String {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            self.base_url.to_string()
        } else {
            // Percent-encode each segment but keep slashes.
            let joined = self
                .base_url
                .join(trimmed)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| format!("{}{}", self.base_url, trimmed));
            joined
        }
    }

    async fn request(&self, method: &str, path: &str, body: Option<String>) -> Result<reqwest::Response> {
        let url = self.url_for(path);
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap_or_default(), &url);
        req = req.header(reqwest::header::AUTHORIZATION, &self.auth_header);
        if let Some(b) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/xml").body(b);
        }
        let resp = req.send().await.map_err(|e| Error::WebDav(format!("{method} {path}: {e}")))?;
        Ok(resp)
    }

    pub async fn stat(&self, path: &str) -> Result<Option<ItemStat>> {
        let resp = self.propfind(path, 0).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::WebDav(format!("stat body: {e}")))?;
        if !status.is_success() {
            return Err(Error::WebDav(format!("PROPFIND {path}: HTTP {status}")));
        }
        let items = parse_multistatus(&text)?;
        Ok(items.into_iter().next())
    }

    pub async fn propfind(&self, path: &str, depth: u32) -> Result<reqwest::Response> {
        let body = r#"<?xml version="1.0"?>\n<d:propfind xmlns:d="DAV:"><d:prop><d:getlastmodified/><d:resourcetype/></d:prop></d:propfind>"#;
        let _ = body;
        let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:getlastmodified/><d:resourcetype/></d:prop></d:propfind>"#;
        let resp = self
            .request("PROPFIND", path, Some(body.to_string()))
            .await?
            .error_for_status()
            .map_err(|e| Error::WebDav(format!("PROPFIND {path}: {e}")))?;
        let _ = depth;
        Ok(resp)
    }

    pub async fn list(&self, path: &str) -> Result<Vec<ItemStat>> {
        let resp = self.propfind(path, 1).await?;
        let text = resp.text().await.map_err(|e| Error::WebDav(format!("list body: {e}")))?;
        parse_multistatus(&text)
    }

    pub async fn get(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let resp = self.request("GET", path, None).await?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => {
                let bytes = resp.bytes().await.map_err(|e| Error::WebDav(format!("get body: {e}")))?;
                Ok(Some(bytes.to_vec()))
            }
            s => Err(Error::WebDav(format!("GET {path}: HTTP {s}"))),
        }
    }

    pub async fn get_text(&self, path: &str) -> Result<Option<String>> {
        let bytes = self.get(path).await?;
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    pub async fn put(&self, path: &str, body: &str) -> Result<()> {
        let resp = self.request("PUT", path, Some(body.to_string())).await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::CREATED {
            return Err(Error::WebDav(format!("PUT {path}: HTTP {}", resp.status())));
        }
        Ok(())
    }

    pub async fn mkcol(&self, path: &str) -> Result<()> {
        let resp = self.request("MKCOL", path, None).await?;
        let s = resp.status();
        if s.is_success() || s == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Err(Error::WebDav(format!("MKCOL {path}: HTTP {s}")))
        }
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let resp = self.request("DELETE", path, None).await?;
        let s = resp.status();
        if s.is_success() || s == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Error::WebDav(format!("DELETE {path}: HTTP {s}")))
        }
    }
}

fn local_name(tag: &[u8]) -> String {
    let s = String::from_utf8_lossy(tag);
    let name = s.rsplit(':').next().unwrap_or(&s);
    name.to_ascii_lowercase()
}

/// Parse a WebDAV multistatus response into ItemStats.
/// Paths returned are relative to the request root, converted to the same
/// "directory path" convention Joplin uses (leading slash stripped, decoded).
pub fn parse_multistatus(xml: &str) -> Result<Vec<ItemStat>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut current_href: Option<String> = None;
    let mut current_is_dir = false;
    let mut current_mtime: i64 = 0;
    let mut in_propstat_status_404 = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "response" => {
                        current_href = None;
                        current_is_dir = false;
                        current_mtime = 0;
                        in_propstat_status_404 = false;
                    }
                    "href" => {
                        if current_href.is_none() {
                            let text = reader.read_text(e.name()).unwrap_or_default();
                            current_href = Some(decode_href(&text));
                        }
                    }
                    "collection" => current_is_dir = true,
                    "status" => {
                        let text = reader.read_text(e.name()).unwrap_or_default();
                        if text.contains(" 404 ") {
                            in_propstat_status_404 = true;
                        }
                    }
                    "getlastmodified" => {
                        let text = reader.read_text(e.name()).unwrap_or_default();
                        current_mtime = parse_http_date(&text).unwrap_or(0);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                // Self-closing tags like <d:collection/>
                let name = local_name(e.name().as_ref());
                if name == "collection" {
                    current_is_dir = true;
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    if let Some(href) = current_href.take() {
                        if !in_propstat_status_404 {
                            items.push(ItemStat { path: href, is_dir: current_is_dir, updated_time: current_mtime });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::WebDav(format!("xml parse: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(items)
}

fn decode_href(href: &str) -> String {
    let decoded = percent_decode(href);
    decoded.trim_matches('/').to_string()
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            if i + 2 < bytes.len() {
                let hex = &s[i + 1..i + 3];
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_http_date(s: &str) -> Option<i64> {
    // RFC 1123: Mon, 02 Jan 2024 03:04:05 GMT
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc2822).ok()?;
    Some(t.unix_timestamp() * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multistatus_nextcloud_style() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/user/joplin/</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Tue, 24 Sep 2024 10:11:12 GMT</d:getlastmodified>
    <d:resourcetype><d:collection/></d:resourcetype>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/user/joplin/0f1e2d3c4b5a69788796a5b4c3d2e1f0.md</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Tue, 24 Sep 2024 10:11:13 GMT</d:getlastmodified>
    <d:resourcetype/>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_dir);
        assert_eq!(items[0].path, "remote.php/dav/files/user/joplin");
        assert!(!items[1].is_dir);
        assert_eq!(items[1].path, "remote.php/dav/files/user/joplin/0f1e2d3c4b5a69788796a5b4c3d2e1f0.md");
        assert!(items[1].updated_time > 0);
    }

    #[test]
    fn parse_handles_uppercase_namespace() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/a/b.md</D:href><D:propstat><D:prop><D:getlastmodified>Thu, 01 Jan 2026 00:00:00 GMT</D:getlastmodified><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "a/b.md");
    }

    #[test]
    fn url_for_joins_paths() {
        let client = WebDavClient::new("http://localhost:1234/dav/", "u", "p").unwrap();
        assert_eq!(client.url_for("info.json"), "http://localhost:1234/dav/info.json");
        assert_eq!(client.url_for(""), "http://localhost:1234/dav/");
    }
}
