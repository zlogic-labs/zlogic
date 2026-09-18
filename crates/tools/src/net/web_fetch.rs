//! `web_fetch` — GET a URL and return its text.
//! # The cap is on bytes read, not bytes kept
//! The body is read chunk by chunk and the connection is dropped at [`MAX_BYTES`]. Reading it all
//! and then trimming would let a page with a wrong (or absent) `content-length` decide how much
//! memory this process uses.
//! # HTML is stripped, not parsed
//! A tag-strip with a handful of entity replacements. It is not a parser and does not pretend to
//! be one — it produces readable prose out of ordinary pages, which is what a model needs, and a
//! real DOM would not make the result meaningfully better for that use.
//! # Everything that is not text is refused
//! A PDF or an image decoded as UTF-8 is a screenful of replacement characters that costs tokens
//! and says nothing. The `content-type` is checked before any of the body is decoded.
//! # Private addresses are out of reach
//! See [`ReachGuard`]. It is the one decision here that the tool makes rather than reports, for
//! the same reason shell scrubs credentials out of a child's environment: what this process is
//! willing to *reach* cannot be settled from the URL text a policy layer gets to see.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::{Recovery, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk, parse_args};

const MAX_BYTES: usize = 256 * 1024;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The limit reqwest's default policy applies, restated because [`redirect::Policy::custom`]
/// replaces that default — with no limit of our own a redirect loop would spin until [`TIMEOUT`].
const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Deserialize)]
struct Args {
    url: String,
}

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn meta(&self) -> ToolMeta {
        // Read, not High: it changes nothing. Reaching the network is the approval pipeline's
        // concern, and it can see the URL — a static risk level cannot.
        ToolMeta {
            name: "web_fetch".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch a URL with HTTP GET and return its text, with HTML tags removed. \
                          Reads at most 256KB, gives up after 15 seconds, and only accepts text, \
                          HTML, JSON and XML responses. Reaches public addresses only — never \
                          this machine, a private network, or a link-local address."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "minLength": 1, "description": "Full URL; must be http:// or https://" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let a: Args = parse_args(args)?;
        fetch(ctx, a.url.trim(), Arc::new(ReachGuard::default())).await
    }
}

/// The fetch itself, taking its [`ReachGuard`] rather than making one, so a test can hand it a
/// guard that exempts the single origin the test is able to serve from.
async fn fetch(ctx: &ToolCtx, url: &str, guard: Arc<ReachGuard>) -> Result<ToolExecResult> {
    if ctx.is_cancelled() {
        return Ok(ToolExecResult::cancelled("fetch interrupted"));
    }
    // Checked here rather than left to the client so the refusal names the actual problem: a
    // `file://` or `ftp://` URL would otherwise come back as an opaque transport error.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(ToolExecResult::failed(format!(
            "{url:?} is not an http:// or https:// URL"
        )));
    }
    let parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(e) => {
            return Ok(ToolExecResult::failed(format!(
                "{url:?} is not a valid URL: {e}"
            )));
        }
    };
    if let Some(message) = guard.admit(&parsed, &format!("the requested URL {url}")) {
        return Ok(ToolExecResult::failed(message));
    }

    let redirects = {
        let guard = Arc::clone(&guard);
        redirect::Policy::custom(move |attempt| {
            let hop_index = attempt.previous().len();
            if hop_index >= MAX_REDIRECTS {
                let message = format!(
                    "gave up after {MAX_REDIRECTS} redirects, the last to {}",
                    attempt.url()
                );
                guard.record(message.clone());
                return attempt.error(message);
            }
            let hop = match attempt.previous().last() {
                Some(from) => format!("redirect hop {hop_index}, {from} → {}", attempt.url()),
                None => format!("redirect hop {hop_index} to {}", attempt.url()),
            };
            // Bound before the match: `follow` and `error` consume `attempt`, and a borrow taken
            // inside the scrutinee would still be alive in the arms.
            let verdict = guard.admit(attempt.url(), &hop);
            match verdict {
                Some(message) => {
                    guard.record(message.clone());
                    attempt.error(message)
                }
                None => attempt.follow(),
            }
        })
    };

    let builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // Some sites reject an empty user agent outright, so identifying ourselves is what
        // makes an ordinary page fetchable at all.
        .user_agent("zlogic/0.1 (web_fetch)")
        // Both halves of the guard: the resolver judges hostnames, the policy judges each hop's
        // address literal and keeps the chain finite.
        .dns_resolver(Arc::new(GuardResolver(Arc::clone(&guard))))
        .redirect(redirects);
    // A test's server is on loopback, and a proxy in the developer's environment would otherwise
    // intercept the request and answer for it.
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolExecResult::failed(format!(
                "cannot build an HTTP client: {e}"
            )));
        }
    };

    let request = client
        .get(url)
        .header(
            "accept",
            "text/html,text/plain,application/json;q=0.9,*/*;q=0.1",
        )
        .send();

    // Cancellation covers the request itself, not only the loop around it: a fetch nobody is
    // waiting for should not keep a connection open.
    let response = tokio::select! {
        r = request => r,
        _ = ctx.cancel.cancelled() => return Ok(ToolExecResult::cancelled("fetch interrupted")),
    };
    let mut response = match response {
        Ok(r) => r,
        Err(e) => {
            // A guard refusal arrives here as an ordinary transport failure, and reqwest's own
            // text for it says only that the request could not be sent.
            if let Some(message) = guard.refused() {
                return Ok(ToolExecResult::failed(message));
            }
            if e.is_timeout() {
                return Ok(ToolExecResult::failed(format!(
                    "{url} did not respond within {}s",
                    TIMEOUT.as_secs()
                )));
            }
            return Ok(ToolExecResult::failed(format!("cannot fetch {url}: {e}")));
        }
    };

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if !status.is_success() {
        // A little of the error body: an API's own message is usually far more useful than
        // the status line, and often the only thing that explains the failure.
        let body = read_capped(&mut response, 2048).await;
        return Ok(ToolExecResult::failed(format!(
            "{url} returned HTTP {}{}",
            status.as_u16(),
            match body.text.trim() {
                "" => String::new(),
                b => format!(": {}", strip_html(b)),
            }
        )));
    }
    if !is_textual(&content_type) {
        return Ok(ToolExecResult::failed(format!(
            "{url} returned {}, which is not text this tool can read",
            if content_type.is_empty() {
                "no content type"
            } else {
                &content_type
            }
        )));
    }

    let final_url = response.url().to_string();
    let body = read_capped(&mut response, MAX_BYTES).await;
    let truncated = body.truncated;
    let text = if content_type.contains("html") {
        strip_html(&body.text)
    } else {
        body.text
    };

    let mut header = format!(
        "requested_url: {url}\nfinal_url: {final_url}\nstatus: {}\ncontent_type: {}",
        status.as_u16(),
        if content_type.is_empty() {
            "(missing)"
        } else {
            &content_type
        }
    );
    if truncated {
        header.push_str(&format!("\ntruncated: stopped at {}KB", MAX_BYTES / 1024));
    }
    if body.stream_error {
        header.push_str("\npartial: the response stream ended with an error");
    }
    ctx.offload_if_large(
        &format!("{header}\n\n{}", text.trim()),
        Recovery::Unavailable,
    )
}

/// Which addresses a fetch is allowed to reach, judged per hop.
/// A URL is model-supplied input, and the approval pipeline judges the URL it can see: `web_fetch`
/// is [`ToolRisk::Read`] and rule-ALLOWed, so a public URL is approved once and the server on the
/// other end then chooses the next address. One `302` to `169.254.169.254` is a cloud instance's
/// credentials; one to `127.0.0.1:<port>` is whatever else this machine is serving. Neither
/// address ever appears in anything the approval pipeline was shown, so no policy layer can be the
/// one to catch it. Hence a floor in the tool, not a setting: a knob that could re-admit these
/// would defeat the point of having them out of reach.
/// The judgement is on the address a connection would actually use — not on the text of the URL,
/// which says nothing about where its hostname points.
#[derive(Default)]
struct ReachGuard {
    /// The hosts this fetch is reaching for, each with the phrase naming its hop in a refusal.
    /// Only these are judged: a proxy's own hostname is resolved through the same resolver, and on
    /// a corporate network that host is legitimately a private address.
    targets: Mutex<HashMap<String, String>>,
    /// The first refusal. Kept because reqwest reports a rejection from the resolver or the
    /// redirect policy as a generic transport error, which would otherwise be all the model sees.
    refusal: Mutex<Option<String>>,
    /// A `host:port` a test may reach in spite of the rules. Every address a test can bind is one
    /// this guard exists to refuse, so a test needs exactly one exemption to have something to
    /// serve from — and because it is a single origin, the hop *after* it is still judged by the
    /// real rule, which is the thing under test.
    #[cfg(test)]
    exempt_origin: Option<String>,
}

impl ReachGuard {
    /// Judges one hop before it is requested, returning the refusal to report.
    /// A hostname is not settled here. Resolving it at this point would only produce a check the
    /// connection is free to contradict a moment later, so names are registered and judged in
    /// [`GuardResolver`], where the addresses in hand are the ones about to be connected to.
    fn admit(&self, url: &Url, hop: &str) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Some(refusal(
                hop,
                format!("{} is not an http:// or https:// URL", url.scheme()),
            ));
        }
        #[cfg(test)]
        if let Some(exempt) = &self.exempt_origin {
            let port = url.port_or_known_default().unwrap_or_default();
            if url
                .host_str()
                .is_some_and(|h| format!("{h}:{port}") == *exempt)
            {
                return None;
            }
        }
        let Some(host) = url.host_str() else {
            return Some(refusal(hop, format!("{url} has no host")));
        };
        // `host_str` keeps the brackets an IPv6 literal is written with inside a URL.
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        match host.parse::<IpAddr>() {
            // An address literal never reaches the resolver — hyper connects to it directly — so
            // this is the only place it can be caught.
            Ok(addr) => blocked_reason(addr).map(|why| refusal(hop, format!("{addr} is {why}"))),
            Err(_) => {
                self.targets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(host, hop.into());
                None
            }
        }
    }

    /// How a refusal should name this host's hop, or `None` for a host this fetch never asked for.
    fn target(&self, host: &str) -> Option<String> {
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(host)
            .cloned()
    }

    fn record(&self, message: String) {
        // First one wins: reqwest may retry a hop against another address family, and the second
        // refusal would only restate the first.
        let mut slot = self.refusal.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert(message);
    }

    fn refused(&self) -> Option<String> {
        self.refusal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Where a *hostname* is judged.
/// hyper asks the resolver for every name it is about to connect to — on the first request and on
/// every redirect — and what the resolver returns *is* the set of addresses the connection will
/// try. So filtering here makes a name that points into a private network unreachable, rather than
/// merely checked: there is no second resolution afterwards that could answer differently.
/// One case this cannot cover: with a proxy configured, the target host is resolved by the proxy
/// and never passes through here. Address literals are still refused, and a proxy is the operator's
/// own deliberate configuration rather than something a URL can introduce.
struct GuardResolver(Arc<ReachGuard>);

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        // The future outlives this call, so it owns its handle to the guard.
        let guard = Arc::clone(&self.0);
        Box::pin(async move {
            let host = name.as_str().to_ascii_lowercase();
            // Port 0: reqwest substitutes the URL's port, or the scheme's default.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(hop) = guard.target(&host) {
                for addr in &addrs {
                    // One blocked answer refuses the whole name. A name resolving to both a public
                    // and a private address is either broken or aimed at this guard, and taking the
                    // public one would leave the outcome to resolver ordering.
                    if let Some(why) = blocked_reason(addr.ip()) {
                        let message = refusal(
                            &hop,
                            format!("{host} resolves to {}, which is {why}", addr.ip()),
                        );
                        guard.record(message.clone());
                        return Err(message.into());
                    }
                }
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The one place a refusal is worded, so every one of them names its hop and its address.
fn refusal(hop: &str, detail: String) -> String {
    format!(
        "refused {hop}: {detail}. web_fetch reaches public addresses only — never this machine, \
         a private network, or a link-local address such as cloud instance metadata."
    )
}

/// Why an address is out of reach, or `None` when there is no reason to refuse it.
fn blocked_reason(addr: IpAddr) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => blocked_v4(v4),
        // `::1` and `::` are IPv6 addresses in their own right. Anything else that carries an IPv4
        // address inside it (`::ffff:127.0.0.1`, `::127.0.0.1`) is judged as that IPv4 address, or
        // the v6 spelling would be a way around every rule below it.
        IpAddr::V6(v6) if v6.is_loopback() => Some("a loopback address"),
        IpAddr::V6(v6) if v6.is_unspecified() => Some("not a routable address"),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped().or_else(|| ipv4_compatible(v6)) {
            Some(v4) => blocked_v4(v4),
            None => blocked_v6(v6),
        },
    }
}

fn blocked_v4(addr: Ipv4Addr) -> Option<&'static str> {
    let o = addr.octets();
    if addr.is_loopback() {
        Some("a loopback address")
    } else if addr.is_private() {
        Some("a private address")
    } else if addr.is_link_local() {
        // 169.254/16 — where every major cloud serves instance credentials.
        Some("a link-local address")
    } else if o[0] == 100 && (64..128).contains(&o[1]) {
        // 100.64/10, which a machine behind carrier NAT shares with its neighbours.
        Some("a carrier-NAT address")
    } else if o[0] == 0 || addr.is_broadcast() {
        // 0.0.0.0 is a local alias for the loopback on most stacks; the rest of 0/8 and the
        // broadcast address are not somewhere a GET can meaningfully go at all.
        Some("not a routable address")
    } else {
        None
    }
}

/// The v6 equivalents, written out because `std`'s predicates for them are still unstable.
fn blocked_v6(addr: Ipv6Addr) -> Option<&'static str> {
    let head = addr.segments()[0];
    if head & 0xffc0 == 0xfe80 {
        Some("a link-local address")
    } else if head & 0xfe00 == 0xfc00 {
        Some("a unique-local address")
    } else if head & 0xffc0 == 0xfec0 {
        // Deprecated site-local, but old stacks still route it.
        Some("a site-local address")
    } else {
        None
    }
}

/// The deprecated `::a.b.c.d` form, which `to_ipv4_mapped` deliberately does not accept.
fn ipv4_compatible(addr: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = addr.segments();
    (s[..6].iter().all(|&x| x == 0)).then(|| {
        Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        )
    })
}

/// Reads the body until `cap` bytes, then stops and drops the rest.
/// Lossy decoding on purpose: a page whose declared charset lies, or that is truncated mid
/// character by the cap, still yields readable text. Failing on it would be worse — a single bad
/// byte would lose the whole document.
struct CappedRead {
    text: String,
    truncated: bool,
    stream_error: bool,
}

async fn read_capped(response: &mut reqwest::Response, cap: usize) -> CappedRead {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream_error = false;
    while buf.len() <= cap {
        match response.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            // A stream that breaks mid-body still yields what arrived — a partial page is worth
            // more than an error, and the caller can see it is partial.
            Ok(None) => break,
            Err(_) => {
                stream_error = true;
                break;
            }
        }
    }
    let truncated = buf.len() > cap;
    buf.truncate(cap);
    CappedRead {
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
        stream_error,
    }
}

fn is_textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("application/json")
        || content_type.contains("application/xml")
        || content_type.contains("application/xhtml")
        || content_type.contains("+json")
        || content_type.contains("+xml")
}

/// HTML to something a model can read.
/// `script` and `style` bodies are removed *before* tags, because stripping tags first would leave
/// their contents behind as prose — which is how a page's minified bundle ends up in the context.
fn strip_html(html: &str) -> String {
    let mut s = remove_blocks(html, "<script", "</script>");
    s = remove_blocks(&s, "<style", "</style>");
    s = remove_blocks(&s, "<!--", "-->");

    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                // A line break for a *closing* block tag, so a list stops being one run-on line.
                // Only the closing one: breaking on both would double-space every element, and a
                // `<br>` has no closing tag so it is named explicitly.
                let name = tag
                    .trim_start_matches('/')
                    .split([' ', '/'])
                    .next()
                    .unwrap_or("");
                let closing = tag.starts_with('/');
                let block = matches!(
                    name,
                    "p" | "div"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "article"
                        | "section"
                        | "ul"
                        | "ol"
                        | "table"
                        | "pre"
                        | "blockquote"
                );
                if name == "br" || (closing && block) {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }

    let decoded = decode_entities(&out);
    // Drop the blank lines that HTML source formatting leaves behind. They carry no information
    // once the tags are gone — the block-tag breaks above are what preserved the structure — and
    // keeping them would double-space the whole document.
    let mut result = String::with_capacity(decoded.len());
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result.trim().to_string()
}

/// Removes every `open … close` span, including unterminated trailing ones.
fn remove_blocks(input: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.to_ascii_lowercase().find(open) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match rest.to_ascii_lowercase().find(close) {
            Some(end) => rest = &rest[end + close.len()..],
            // An unclosed `<script` means the rest of the document is script. Dropping it is
            // right: keeping it would put code into the context as prose.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The handful of entities that actually appear in prose, plus numeric references.
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // An entity is short; a bare `&` in the text is far more common than a broken one, so the
        // search window is bounded rather than scanning to the next `;` anywhere in the document.
        // The window is a byte range, and UTF-8 must not be cut mid-character: the bound is
        // shortened to the nearest char boundary (12 bytes is a heuristic, an entity is shorter).
        let mut window_end = rest.len().min(12);
        while !rest.is_char_boundary(window_end) {
            window_end -= 1;
        }
        let Some(semi) = rest[..window_end].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..semi];
        let replacement = match name.to_ascii_lowercase().as_str() {
            "nbsp" => Some(" ".to_string()),
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" => Some("'".to_string()),
            "hellip" => Some("…".to_string()),
            "mdash" => Some("—".to_string()),
            "ndash" => Some("–".to_string()),
            n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(String::from),
            n if n.starts_with('#') => n[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(String::from),
            _ => None,
        };
        match replacement {
            Some(r) => {
                out.push_str(&r);
                rest = &rest[semi + 1..];
            }
            // Unrecognised: left exactly as written. Guessing would corrupt the text.
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecStatus, test_ctx};

    fn ctx() -> ToolCtx {
        let mut c = test_ctx(std::path::Path::new("/work"));
        c.max_result_chars = 100_000;
        c
    }

    /// A bare `&` followed by multibyte text must not panic: the bounded entity window is a byte
    /// range, and cutting it mid-character used to panic on any non-ASCII page containing a stray
    /// `&` (reproduced with weather.com.cn, where the panic killed the turn silently).
    #[test]
    fn a_bare_ampersand_before_multibyte_text_does_not_panic() {
        // The first `&` has no `;` within 12 bytes and is followed by CJK (3 bytes per char), so
        // the old slice `rest[..rest.len().min(12)]` cut through a character.
        let input = "&天气晴朗&nbsp;今日";
        // The stray `&` survives verbatim; the real entity still decodes.
        assert_eq!(decode_entities(input), "&天气晴朗 今日");
    }

    /// A non-HTTP scheme is named, not reported as a transport failure.
    #[tokio::test]
    async fn a_non_http_url_is_refused_with_a_useful_message() {
        for url in ["file:///etc/passwd", "ftp://example.com", "example.com"] {
            let out = WebFetch
                .execute(&ctx(), &json!({ "url": url }).to_string())
                .await
                .unwrap();
            assert_eq!(out.status, ToolExecStatus::Failed, "{url}");
            assert!(out.model_text().contains("http"), "{}", out.model_text());
        }
    }

    /// No network in tests: the failure must be a result the model can act on, not an `Err`.
    #[tokio::test]
    async fn an_unreachable_host_is_a_failed_result_not_an_err() {
        // A `.invalid` name never resolves anywhere, and it is a *name*, so it reaches the
        // resolver rather than being refused as a literal — which is the path under test.
        let url = "http://nothing-here.invalid/page";
        let out = WebFetch
            .execute(&ctx(), &json!({ "url": url }).to_string())
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(
            out.model_text().contains("nothing-here.invalid"),
            "{}",
            out.model_text()
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_does_not_wait_for_the_response() {
        let c = ctx();
        c.cancel.cancel();
        // An address that would otherwise hang until the timeout. It is also a private address, so
        // the guard is told to allow this one origin — the point here is the cancellation.
        let out = fetch(&c, "http://10.255.255.1/slow", exempting("10.255.255.1:80"))
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Cancelled);
    }

    /// A URL naming a private address is refused before a connection is attempted, whatever
    /// approved it: the approval pipeline sees `web_fetch` as a read and allows it by rule.
    #[tokio::test]
    async fn an_address_this_process_will_not_reach_is_refused_up_front() {
        for (url, why) in [
            ("http://127.0.0.1:8080/admin", "loopback"),
            ("http://169.254.169.254/latest/meta-data/iam/", "link-local"),
            ("http://10.1.2.3/internal", "private"),
            ("http://192.168.0.1/", "private"),
            ("http://172.16.9.9/", "private"),
            ("http://100.64.0.1/", "carrier-NAT"),
            ("http://0.0.0.0:9200/", "not a routable address"),
            ("http://[::1]:9200/_cat/indices", "loopback"),
            ("http://[fe80::1]/", "link-local"),
            ("http://[fd00::1]/", "unique-local"),
            // The v6 spelling of the loopback must not be a way around the v4 rule.
            ("http://[::ffff:127.0.0.1]/", "loopback"),
        ] {
            let out = WebFetch
                .execute(&ctx(), &json!({ "url": url }).to_string())
                .await
                .unwrap();
            assert_eq!(out.status, ToolExecStatus::Failed, "{url}");
            let text = out.model_text();
            assert!(
                text.starts_with("refused the requested URL"),
                "{url}: {text}"
            );
            assert!(text.contains(why), "{url}: {text}");
        }
    }

    /// The case a URL check alone cannot catch: the model asks for a host it is allowed to reach,
    /// and that host answers with a redirect into this machine.
    #[tokio::test]
    async fn a_redirect_into_loopback_is_refused_at_the_hop_that_asked_for_it() {
        let origin = serve_once(
            "HTTP/1.1 302 Found\r\n\
             Location: http://127.0.0.1:9/secret\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .await;
        let url = format!("http://{origin}/redirect");

        let out = fetch(&ctx(), &url, exempting(&origin)).await.unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        // Which hop, and which address on it.
        assert!(text.contains("redirect hop 1"), "{text}");
        assert!(text.contains("http://127.0.0.1:9/secret"), "{text}");
        assert!(text.contains("loopback"), "{text}");
    }

    /// The other half of the guard: nothing in `http://localhost:…` looks like an address, so only
    /// the resolver can refuse it — and that is where a name pointing into a private network, or
    /// answering differently the second time it is asked, has to be caught.
    #[tokio::test]
    async fn a_hostname_that_resolves_into_loopback_is_refused_at_the_resolver() {
        let url = "http://localhost:9/secret";
        let out = WebFetch
            .execute(&ctx(), &json!({ "url": url }).to_string())
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.starts_with("refused the requested URL"), "{text}");
        // Not asserted as an exact address: `localhost` is `::1` first on some machines.
        assert!(text.contains("localhost resolves to"), "{text}");
        assert!(text.contains("loopback"), "{text}");
    }

    /// The same, one hop in: the refusal still has to say which hop asked for it.
    #[tokio::test]
    async fn a_redirect_to_a_hostname_resolving_into_loopback_names_its_hop() {
        let origin = serve_once(
            "HTTP/1.1 302 Found\r\n\
             Location: http://localhost:9/secret\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .await;
        let url = format!("http://{origin}/redirect");

        let out = fetch(&ctx(), &url, exempting(&origin)).await.unwrap();

        assert_eq!(out.status, ToolExecStatus::Failed);
        let text = out.model_text();
        assert!(text.contains("redirect hop 1"), "{text}");
        assert!(text.contains("localhost resolves to"), "{text}");
        assert!(text.contains("loopback"), "{text}");
    }

    /// The guard must not have turned an ordinary fetch into a refusal.
    #[tokio::test]
    async fn a_permitted_origin_is_still_fetched_and_returned() {
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 22\r\n\r\n\
             hello from the network",
        )
        .await;
        let url = format!("http://{origin}/page");

        let out = fetch(&ctx(), &url, exempting(&origin)).await.unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert!(
            out.model_text().contains("hello from the network"),
            "{}",
            out.model_text()
        );
    }

    /// What the refusal tests cannot show: that a name the guard *admits* still resolves to
    /// something the connection can use. The resolver returns port 0 and relies on reqwest to
    /// substitute the URL's port — if that convention changed, every fetch by hostname would fail
    /// and every other test here would still pass.
    #[tokio::test]
    async fn an_admitted_hostname_resolves_to_an_address_the_connection_can_use() {
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: 12\r\n\r\n\
             served by ip",
        )
        .await;
        let port = origin.rsplit(':').next().unwrap().to_string();
        let url = format!("http://localhost:{port}/page");

        let out = fetch(&ctx(), &url, exempting(&format!("localhost:{port}")))
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success, "{}", out.model_text());
        assert!(
            out.model_text().contains("served by ip"),
            "{}",
            out.model_text()
        );
    }

    #[test]
    fn a_public_address_carries_no_reason_to_refuse_it() {
        for addr in [
            "1.1.1.1",
            "93.184.216.34",
            // Just outside the ranges above, which is where an over-wide mask would show up.
            "172.32.0.1",
            "100.128.0.1",
            "169.253.0.1",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            assert_eq!(blocked_reason(addr.parse().unwrap()), None, "{addr}");
        }
        for addr in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "fd12::1",
            "::127.0.0.1",
        ] {
            assert!(blocked_reason(addr.parse().unwrap()).is_some(), "{addr}");
        }
    }

    fn exempting(origin: &str) -> Arc<ReachGuard> {
        Arc::new(ReachGuard {
            exempt_origin: Some(origin.to_string()),
            ..Default::default()
        })
    }

    /// Answers one request with `response`, then stops. Returns the `host:port` to fetch.
    /// A real listener rather than a stub transport: the redirect policy and the resolver are
    /// reqwest's own hooks, so only a real client exercises them in the order they run.
    async fn serve_once(response: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Read once and discard: the response is canned, and what matters is only that the
            // request arrived before it is written.
            let _ = stream.read(&mut [0u8; 2048]).await;
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        origin
    }

    #[tokio::test]
    async fn missing_required_args_are_rejected() {
        assert!(WebFetch.execute(&ctx(), "{}").await.is_err());
        assert!(WebFetch.execute(&ctx(), "not json").await.is_err());
    }

    #[test]
    fn only_textual_content_types_are_accepted() {
        for ct in [
            "text/html; charset=utf-8",
            "text/plain",
            "application/json",
            "application/xml",
            "application/vnd.api+json",
        ] {
            assert!(is_textual(ct), "{ct}");
        }
        for ct in [
            "application/pdf",
            "image/png",
            "application/octet-stream",
            "",
        ] {
            assert!(!is_textual(ct), "{ct}");
        }
    }

    /// The ordering that matters: script bodies go before tags, or the bundle becomes prose.
    #[test]
    fn script_and_style_bodies_are_removed_not_unwrapped() {
        let html = "<html><head><style>body{color:red}</style>\
                    <script>var secret = 'do not read me';</script></head>\
                    <body><p>Real content</p></body></html>";
        let text = strip_html(html);
        assert_eq!(text, "Real content");
        assert!(!text.contains("color"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    /// An unclosed `<script` means the remainder is code, and code must not reach the context.
    #[test]
    fn an_unterminated_script_block_takes_the_rest_with_it() {
        let text = strip_html("<p>before</p><script>var x = 1; // never closed");
        assert_eq!(text, "before");
    }

    #[test]
    fn block_tags_become_line_breaks_so_structure_survives() {
        let text = strip_html("<ul><li>one</li><li>two</li></ul><p>after</p>");
        assert_eq!(text, "one\ntwo\nafter");
    }

    #[test]
    fn inline_tags_do_not_break_a_sentence() {
        let text = strip_html("<p>a <em>very</em> <strong>good</strong> point</p>");
        assert_eq!(text, "a very good point");
    }

    #[test]
    fn the_common_entities_decode_and_the_rest_are_left_alone() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&#39;quoted&#39;"), "'quoted'");
        assert_eq!(decode_entities("&#x2014;"), "—");
        // Unknown entities and bare ampersands must survive verbatim rather than be guessed at.
        assert_eq!(decode_entities("&notarealentity; x"), "&notarealentity; x");
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("a & b; c"), "a & b; c");
    }

    /// Source formatting contributes nothing once the tags are gone.
    #[test]
    fn blank_lines_from_the_html_source_are_dropped() {
        assert_eq!(strip_html("<p>one</p>\n\n\n\n<p>two</p>"), "one\ntwo");
        assert_eq!(strip_html("  <p>  padded  </p>  "), "padded");
    }
}
