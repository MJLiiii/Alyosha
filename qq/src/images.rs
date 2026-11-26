//! Inbound attachments from the official QQ Bot API v2, not URLs in chat text.

use std::{io, net::IpAddr, time::Duration};

use alyosha_bridge::PromptImage;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::future::join_all;
use reqwest::{Client, Response, Url, dns, header::CONTENT_TYPE};
use serde_json::Value;

const CDN_HOSTS: &[&str] = &[
    "multimedia.nt.qq.com.cn",
    "multimedia.nt.qq.com",
    "gchat.qpic.cn",
    "c2cpicdw.qpic.cn",
];
const MAX_IMAGES: usize = 4;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_URL_BYTES: usize = 4096;

/// Candidates only. The server response, not the event, supplies the bytes.
#[derive(Clone, Default)]
pub(crate) struct Attachments {
    pub(crate) present: bool,
    candidates: Vec<Candidate>,
    omitted: bool,
}

// Signed links must not end up in logs when a gateway message is debug-printed.
impl std::fmt::Debug for Attachments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachments")
            .field("present", &self.present)
            .field("candidates", &self.candidates.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}

#[derive(Clone)]
struct Candidate {
    url: Url,
    mime_type: Option<&'static str>,
}

impl Attachments {
    pub(crate) fn parse(message: &Value) -> Self {
        let mut attachments = Self::default();
        attachments.visit(message, 0, &mut 32);
        attachments
    }

    fn visit(&mut self, message: &Value, depth: usize, remaining: &mut usize) {
        if *remaining == 0 || depth > 4 {
            return;
        }
        *remaining -= 1;
        if let Some(items) = message["attachments"].as_array() {
            self.present |= !items.is_empty();
            self.omitted |= items.len() > 32;
            for item in items.iter().take(32) {
                let declared = item["content_type"].as_str().unwrap_or_default();
                let mime_type = image_mime(declared);
                if mime_type.is_none() && !generic_mime(declared) {
                    self.omitted |= declared.starts_with("image/");
                    continue;
                }
                let Some(url) = item["url"].as_str().and_then(cdn_url) else {
                    self.omitted = true;
                    continue;
                };
                if self.candidates.iter().any(|candidate| candidate.url == url) {
                    continue;
                }
                if self.candidates.len() == MAX_IMAGES
                    || item["size"]
                        .as_u64()
                        .is_some_and(|size| size > MAX_BYTES as u64)
                {
                    self.omitted = true;
                    continue;
                }
                self.candidates.push(Candidate { url, mime_type });
            }
        }
        // QQ delivers quoted (103) and combined messages as nested MsgElements.
        // A bare ref_msg_idx/auth_token is not a URL and is never fetched.
        if depth < 4
            && let Some(elements) = message["msg_elements"].as_array()
        {
            for element in elements {
                if *remaining == 0 {
                    break;
                }
                self.visit(element, depth + 1, remaining);
            }
        }
    }
}

fn image_mime(value: &str) -> Option<&'static str> {
    ["image/jpeg", "image/png", "image/webp"]
        .into_iter()
        .find(|mime| mime.eq_ignore_ascii_case(value))
}

fn generic_mime(value: &str) -> bool {
    value.is_empty() || value.eq_ignore_ascii_case("application/octet-stream")
}

fn cdn_url(raw: &str) -> Option<Url> {
    if raw.is_empty()
        || raw.len() > MAX_URL_BYTES
        || raw.contains(|c: char| c.is_whitespace() || c.is_ascii_control() || c == '\\')
    {
        return None;
    }
    // Older events may supply scheme-less CDN URLs. Never fetch over HTTP.
    let location = match raw.split_once("://") {
        Some((scheme, rest)) if !scheme.contains(['/', '?', '#']) => {
            if !scheme.eq_ignore_ascii_case("https") {
                return None;
            }
            rest
        }
        _ => raw.strip_prefix("//").unwrap_or(raw),
    };
    let authority = location.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let url = Url::parse(&format!("https://{location}")).ok()?;
    (url.port_or_known_default() == Some(443)
        && url.fragment().is_none()
        && url
            .domain()
            .is_some_and(|domain| CDN_HOSTS.contains(&domain)))
    .then_some(url)
}

/// Give the connector only public addresses; never resolve once for validation
/// and a second time for connection. The hostname is still checked by TLS.
struct CdnDns;

impl dns::Resolve for CdnDns {
    fn resolve(&self, name: dns::Name) -> dns::Resolving {
        Box::pin(async move {
            if !CDN_HOSTS.contains(&name.as_str()) {
                return Err(io::Error::other("not a QQ media host").into());
            }
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|addr| public_address(addr.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(io::Error::other("no public QQ media address").into());
            }
            Ok(Box::new(addresses.into_iter()) as dns::Addrs)
        })
    }
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !matches!(
            ip.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 88, 99, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        IpAddr::V6(ip) => {
            matches!(ip.segments()[0], 0x2000..=0x3fff)
                && !matches!(
                    ip.segments(),
                    [0x2001, 0..=0x1ff | 0xdb8, ..] | [0x2002, ..] | [0x3fff, ..]
                )
        }
    }
}

pub(crate) struct Downloader(Client);

impl Downloader {
    pub(crate) fn new() -> Self {
        Self(
            Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(CdnDns)
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(8))
                .build()
                .expect("valid QQ media client settings"),
        )
    }

    /// Preserve attachment order, and keep independently successful downloads.
    pub(crate) async fn download(&self, attachments: &Attachments) -> (Vec<PromptImage>, bool) {
        let mut images = Vec::new();
        let mut omitted = attachments.omitted;
        for result in join_all(
            attachments
                .candidates
                .iter()
                .map(|candidate| self.get(candidate)),
        )
        .await
        {
            match result {
                Ok(image) => images.push(image),
                Err(reason) => {
                    omitted = true;
                    tracing::debug!(reason, "QQ image attachment was skipped");
                }
            }
        }
        (images, omitted)
    }

    async fn get(&self, candidate: &Candidate) -> Result<PromptImage, &'static str> {
        let response = self
            .0
            .get(candidate.url.clone())
            .send()
            .await
            .map_err(|_| "request failed")?;
        read_image(response, candidate.mime_type).await
    }
}

async fn read_image(
    mut response: Response,
    declared: Option<&str>,
) -> Result<PromptImage, &'static str> {
    if !response.status().is_success() {
        return Err("HTTP error or redirect");
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err("image too large");
    }
    let wire = match response.headers().get(CONTENT_TYPE) {
        Some(value) => value
            .to_str()
            .map_err(|_| "invalid Content-Type")?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim(),
        None => "",
    };
    let header_mime = image_mime(wire);
    if header_mime.is_none() && !generic_mime(wire) {
        return Err("not a supported image Content-Type");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "body read failed")? {
        if chunk.len() > MAX_BYTES - bytes.len() {
            return Err("image too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    let mime_type = if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        return Err("unsupported image signature");
    };
    if declared.is_some_and(|value| value != mime_type)
        || header_mime.is_some_and(|value| value != mime_type)
    {
        return Err("image type mismatch");
    }
    Ok(PromptImage {
        mime_type,
        data: STANDARD.encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jKZkAAAAASUVORK5CYII=";

    #[test]
    fn cdn_urls_must_have_an_exact_host_and_use_https() {
        for url in [
            "https://multimedia.nt.qq.com.cn/download?fileid=1&rkey=secret",
            "//gchat.qpic.cn/download.jpg",
            "c2cpicdw.qpic.cn/pic.jpg",
        ] {
            assert_eq!(cdn_url(url).unwrap().scheme(), "https", "{url}");
        }
        for url in [
            "https://127.0.0.1/image",
            "https://[::ffff:127.0.0.1]/image",
            "https://gchat.qpic.cn.example.org/image",
            "https://evil.qpic.cn/image",
            "https://evil.test@gchat.qpic.cn/image",
            "https://@gchat.qpic.cn/image",
            "https://gchat.qpic.cn:8443/image",
            "https:///gchat.qpic.cn/image",
            "https://gchat.qpic.cn/image#fragment",
            "http://gchat.qpic.cn/image",
            "file:///etc/passwd",
        ] {
            assert!(cdn_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn only_public_addresses_are_kept_by_dns() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.168.1.2",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public_address(address.parse().unwrap()), "{address}");
        }
        for address in ["36.155.207.179", "2409:8c20:818:111::12d"] {
            assert!(public_address(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn quoted_images_are_merged_after_direct_images_and_deduplicated() {
        let attachment = |name: &str| json!({"url": format!("https://gchat.qpic.cn/{name}"), "content_type": "image/png"});
        let event = json!({
            "content": "look",
            "message_type": 103,
            "attachments": [attachment("direct"), {"content_type": "voice", "url": "gchat.qpic.cn/voice"}],
            "msg_elements": [{"attachments": [attachment("direct"), attachment("quoted")],
                              "msg_elements": [{"attachments": [attachment("nested"), attachment("fourth"), attachment("over-limit")]}]}],
            "message_scene": {"ext": ["ref_msg_idx=not-a-url"]}
        });
        let attachments = Attachments::parse(&event);
        assert!(attachments.present && attachments.omitted);
        let paths: Vec<_> = attachments
            .candidates
            .iter()
            .map(|item| item.url.path())
            .collect();
        assert_eq!(paths, ["/direct", "/quoted", "/nested", "/fourth"]);
        let oversize = Attachments::parse(
            &json!({"attachments": [{"url": "gchat.qpic.cn/large", "content_type": "image/png", "size": MAX_BYTES + 1}]}),
        );
        assert!(oversize.present && oversize.omitted && oversize.candidates.is_empty());
        let bare_ref = Attachments::parse(
            &json!({"message_type": 103, "message_scene": {"ext": ["ref_msg_idx=not-a-url"]}}),
        );
        assert!(!bare_ref.present && bare_ref.candidates.is_empty());
    }

    // Exercise body validation through reqwest without weakening the live CDN policy.
    async fn serve_response(headers: String, body: Vec<u8>) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(headers.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
        });
        Url::parse(&format!("http://{address}/image")).unwrap()
    }

    #[tokio::test]
    async fn a_good_image_survives_failed_downloads_and_mime_mismatches() {
        let real = STANDARD.decode(PNG).unwrap();
        let good = serve_response(format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", real.len()), real.clone()).await;
        let fake = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\n\r\n".into(),
            b"oops".to_vec(),
        )
        .await;
        let wrong_mime = serve_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                real.len()
            ),
            real,
        )
        .await;
        let redirect = serve_response(
            "HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\nContent-Length: 0\r\n\r\n"
                .into(),
            Vec::new(),
        )
        .await;
        let downloader = Downloader(
            Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        );
        let attachments = Attachments {
            present: true,
            candidates: [fake, good, wrong_mime, redirect]
                .into_iter()
                .map(|url| Candidate {
                    url,
                    mime_type: Some("image/png"),
                })
                .collect(),
            omitted: false,
        };
        let (images, omitted) = downloader.download(&attachments).await;
        assert!(omitted);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, PNG);
    }

    #[tokio::test]
    async fn the_byte_limit_applies_even_without_content_length() {
        let mut body = vec![0; MAX_BYTES + 1];
        body[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let url = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n".into(),
            body,
        )
        .await;
        let reply = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert_eq!(read_image(reply, None).await.err(), Some("image too large"));
    }
}
