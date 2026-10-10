//! `search` - web requests for agents, https only and through the user's rules
//!
//! The rules are a mode and a url list. `allow` lets a request through only
//! when an entry matches it, `block` refuses one when an entry matches it, and
//! with no mode any https url goes. Search endpoints are url templates holding
//! `{query}` - an internal wiki's search page, say - and every request, an
//! endpoint's or a fetched page's, redirects included, passes the same rules.

use globset::{GlobBuilder, GlobMatcher};
use std::process::Command;

// what every refusal starts with, so the rules check can find one in a transcript
pub const REFUSED: &str = "search refused:";
// lines one answer carries before the rest waits behind `offset`
pub const PAGE_LINES: usize = 200;
const FETCH_TIMEOUT_SECS: u64 = 20;
const MAX_BYTES: u64 = 5 << 20;
const MAX_REDIRECTS: usize = 5;
// the lines one endpoint's results add to a search, so one portal cannot crowd out the rest
const ENDPOINT_LINES: usize = 120;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    None,
    Allow,
    Block,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Mode::None),
            "allow" => Ok(Mode::Allow),
            "block" => Ok(Mode::Block),
            other => Err(format!("search mode wants `none`, `allow` or `block`, got '{other}'")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Allow => "allow",
            Mode::Block => "block",
        }
    }
}

// one url list entry: a url glob when it names a scheme, a host glob otherwise
#[derive(Clone, Debug)]
struct Rule {
    entry: String,
    whole_url: bool,
    glob: GlobMatcher,
}

impl Rule {
    fn matches(&self, url: &Url) -> bool {
        if self.whole_url {
            self.glob.is_match(&url.full)
        } else {
            self.glob.is_match(&url.host)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub mode: Mode,
    rules: Vec<Rule>,
    pub endpoints: Vec<String>,
}

impl Policy {
    // every entry checked up front, so a typo stops the server rather than quietly allowing or blocking nothing
    pub fn new(mode: Mode, urls: &[String], endpoints: &[String]) -> Result<Policy, String> {
        let mut rules = Vec::new();
        for raw in urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
            let whole_url = raw.contains("://");
            if !whole_url && raw.contains('/') {
                return Err(format!("search url '{raw}' is neither a url (`https://host/path/**`) nor a host (`*.corp.example`)"));
            }
            let glob = GlobBuilder::new(raw)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .map_err(|e| format!("search url '{raw}' is not a valid pattern: {e}"))?
                .compile_matcher();
            rules.push(Rule { entry: raw.to_string(), whole_url, glob });
        }
        let mut kept = Vec::new();
        for raw in endpoints.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            if !raw.contains("{query}") {
                return Err(format!("search endpoint '{raw}' has no `{{query}}` for the words to go in"));
            }
            parse_url(&raw.replace("{query}", "q")).map_err(|e| format!("search endpoint '{raw}': {e}"))?;
            kept.push(raw.to_string());
        }
        Ok(Policy { mode, rules, endpoints: kept })
    }

    // one line on what the rules are, for startup and every answer
    pub fn describe(&self) -> String {
        let rules = match self.mode {
            Mode::None => "no mode set, so any https url goes".to_string(),
            Mode::Allow => format!("allow - only urls matching one of {} entr(ies) go", self.rules.len()),
            Mode::Block => format!("block - urls matching one of {} entr(ies) are refused", self.rules.len()),
        };
        format!("{rules}; {} search endpoint(s)", self.endpoints.len())
    }

    // the url as it will be requested, or why the rules refuse it
    pub fn check(&self, raw: &str) -> Result<Url, String> {
        let url = parse_url(raw).map_err(|e| format!("{REFUSED} {} - {e}", raw.trim()))?;
        let hit = self.rules.iter().find(|r| r.matches(&url));
        match (self.mode, hit) {
            (Mode::None, _) | (Mode::Allow, Some(_)) | (Mode::Block, None) => Ok(url),
            (Mode::Allow, None) => Err(format!("{REFUSED} {} - not on the allow list", url.full)),
            (Mode::Block, Some(r)) => Err(format!("{REFUSED} {} - matches block entry `{}`", url.full, r.entry)),
        }
    }
}

// an https url, taken apart only as far as the rules need
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    // scheme and host lowercased, a bare host given a `/`, the fragment dropped
    pub full: String,
    pub host: String,
    origin: String,
}

fn parse_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a url cannot hold spaces or control characters".into());
    }
    let (scheme, rest) = raw.split_once("://").ok_or("not an absolute url")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(format!("only https is allowed, not {scheme}"));
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest[..end].to_ascii_lowercase();
    if authority.contains('@') {
        return Err("credentials in a url are refused".into());
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split_once(']').ok_or("an unclosed ipv6 host")?.0),
        None => authority.split(':').next().unwrap_or_default().to_string(),
    };
    if host.is_empty() {
        return Err("no host".into());
    }
    let tail = rest[end..].split('#').next().unwrap_or_default();
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{tail}") };
    let origin = format!("https://{authority}");
    Ok(Url { full: format!("{origin}{path}"), host, origin })
}

// where a link or a redirect points, made absolute against the page it is on
fn resolve(base: &Url, href: &str) -> String {
    let href = href.trim();
    if href.contains("://") {
        return href.to_string();
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    if href.starts_with('/') {
        return format!("{}{href}", base.origin);
    }
    let path = base.full[base.origin.len()..].split(['?', '#']).next().unwrap_or("/");
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    format!("{}{dir}/{href}", base.origin)
}

// the words of a query as a url carries them
fn encode(q: &str) -> String {
    q.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// one answered request
struct Fetched {
    url: String,
    // the url asked for, when redirects ended somewhere else
    from: Option<String>,
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Policy {
    // a GET with every redirect followed by hand, so each hop passes the rules too
    fn fetch(&self, first: Url) -> Result<Fetched, String> {
        let mut url = first.clone();
        for _ in 0..=MAX_REDIRECTS {
            let (status, headers, body) = get(&url.full)?;
            let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.as_str());
            if let (300..=399, Some(next)) = (status, location) {
                url = self.check(&resolve(&url, next))?;
                continue;
            }
            let content_type = headers
                .iter()
                .find(|(k, _)| k == "content-type")
                .map(|(_, v)| v.to_ascii_lowercase())
                .unwrap_or_default();
            let from = (url != first).then(|| first.full.clone());
            return Ok(Fetched { url: url.full, from, status, content_type, body });
        }
        Err(format!("{} redirected more than {MAX_REDIRECTS} times", first.full))
    }
}

// one request by curl - https only, redirects not followed, its status and headers read back
// no http client is linked in for this: curl honours the proxy settings a corporate network relies on
fn get(url: &str) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--include", "--proto", "=https", "--proto-redir", "=https"])
        .args(["--max-time", &FETCH_TIMEOUT_SECS.to_string(), "--max-filesize", &MAX_BYTES.to_string()])
        .args(["--user-agent", concat!("ccc-search/", env!("CARGO_PKG_VERSION"))])
        .arg(url)
        .output()
        .map_err(|e| format!("running curl for {url} (is curl installed?): {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(match out.status.code() {
            Some(63) => format!("{url} is larger than {} MB", MAX_BYTES >> 20),
            _ => format!("fetching {url} failed: {why}"),
        });
    }
    split_response(&out.stdout).ok_or_else(|| format!("{url} answered with something that is not http"))
}

// `--include` output as status, headers and body - a proxy's `200 Connection
// established` and any `100 Continue` are the head of what follows, not the answer
fn split_response(raw: &[u8]) -> Option<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut rest = raw;
    loop {
        let end = rest.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4))
            .or_else(|| rest.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2)))?;
        let head = String::from_utf8_lossy(&rest[..end.0]).to_string();
        let body = &rest[end.1..];
        let mut lines = head.lines();
        let status_line = lines.next()?;
        let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
        let interim = (100..200).contains(&status) || status_line.to_ascii_lowercase().contains("connection established");
        if interim && body.starts_with(b"HTTP/") {
            rest = body;
            continue;
        }
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        return Some((status, headers, body.to_vec()));
    }
}

// what a page says, as lines of text - html read down to its words and links
fn readable(page: &Fetched) -> Result<Vec<String>, String> {
    let ct = page.content_type.as_str();
    let text = String::from_utf8_lossy(&page.body);
    let sniffed_html = || {
        let head = text.trim_start().get(..15).unwrap_or_default().to_ascii_lowercase();
        head.starts_with("<!doctype html") || head.starts_with("<html")
    };
    let body = if ct.contains("html") || (ct.is_empty() && sniffed_html()) {
        html_text(&text, &parse_url(&page.url).map_err(|e| format!("{}: {e}", page.url))?)
    } else if ct.contains("json") {
        serde_json::from_slice::<serde_json::Value>(&page.body)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| text.to_string())
    } else if ct.is_empty() || ct.starts_with("text/") || ct.contains("xml") || ct.contains("markdown") {
        if page.body.contains(&0) {
            return Err(format!("{} is binary, and search reads text pages only", page.url));
        }
        text.to_string()
    } else {
        return Err(format!("{} is {ct}, and search reads text pages only", page.url));
    };
    Ok(body.lines().map(str::to_string).collect())
}

// tags that end a line of text where they open or close
const BLOCKS: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "dd", "div", "dl", "dt", "footer", "form", "h1", "h2", "h3", "h4", "h5",
    "h6", "header", "hr", "main", "nav", "ol", "p", "pre", "section", "table", "tr", "ul",
];
// tags whose content is never text a reader sees
const HIDDEN: &[&str] = &["script", "style", "noscript", "svg", "template", "head"];

// an html page's text, a line per block, its links written `[text](url)` so the agent can fetch the next page
fn html_text(html: &str, base: &Url) -> String {
    let lower = html.to_ascii_lowercase();
    let title = lower
        .find("<title")
        .and_then(|s| Some((s, lower[s..].find('>')? + s + 1)))
        .and_then(|(_, open)| Some((open, lower[open..].find("</title")? + open)))
        .map(|(a, b)| decode(&html[a..b]).split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut out = String::new();
    let mut link: Option<(String, usize)> = None;
    let mut i = 0;
    while i < html.len() {
        if !html[i..].starts_with('<') {
            let next = html[i..].find('<').map_or(html.len(), |n| i + n);
            let raw = decode(&html[i..next]);
            let words = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            // the space between this text and the tag either side of it
            if raw.starts_with(char::is_whitespace) && !out.is_empty() && !out.ends_with([' ', '\n']) {
                out.push(' ');
            }
            out.push_str(&words);
            if raw.ends_with(char::is_whitespace) && !words.is_empty() {
                out.push(' ');
            }
            i = next;
            continue;
        }
        if lower[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map_or(html.len(), |e| i + e + 3);
            continue;
        }
        let end = tag_end(html, i);
        let tag = &html[i + 1..end.min(html.len())];
        i = (end + 1).min(html.len());
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if HIDDEN.contains(&name.as_str()) && !closing {
            i = lower[i..].find(&format!("</{name}")).map_or(html.len(), |e| {
                let close = i + e;
                lower[close..].find('>').map_or(html.len(), |g| close + g + 1)
            });
            continue;
        }
        match name.as_str() {
            "li" if !closing => out.push_str("\n- "),
            "td" | "th" if !closing => out.push_str(" | "),
            "a" if !closing => {
                link = attr(tag, "href")
                    .map(|h| decode(&h))
                    .filter(|h| !h.starts_with('#') && !h.to_ascii_lowercase().starts_with("javascript:") && !h.starts_with("mailto:"))
                    .map(|h| (resolve(base, &h), out.len()));
            }
            "a" => {
                if let Some((href, start)) = link.take() {
                    let text = out[start..].split_whitespace().collect::<Vec<_>>().join(" ");
                    out.truncate(start);
                    if text.is_empty() {
                        out.push_str(&format!("<{href}>"));
                    } else {
                        out.push_str(&format!("[{text}]({href})"));
                    }
                }
            }
            n if BLOCKS.contains(&n) => out.push('\n'),
            _ => {}
        }
    }
    let mut lines: Vec<String> = Vec::new();
    if !title.is_empty() {
        lines.push(format!("# {title}"));
    }
    for line in out.lines().map(str::trim) {
        // one blank line between blocks, never a run of them
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line.to_string());
    }
    lines.join("\n")
}

// the `>` that closes the tag opening at `start`, quotes respected
fn tag_end(html: &str, start: usize) -> usize {
    let mut quote: Option<char> = None;
    for (k, c) in html[start..].char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return start + k,
            _ => {}
        }
    }
    html.len()
}

// an attribute's value inside a tag, quoted or bare
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let at = from + at;
        from = at + name.len();
        let before_ok = at == 0 || lower[..at].ends_with(char::is_whitespace);
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_at = tag.len() - rest.len() + 1;
        let value = tag[value_at..].trim_start();
        return Some(match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default().to_string(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or_default().to_string(),
        });
    }
    None
}

// the character references a page's text is written with
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(semi) = rest.bytes().take(12).position(|b| b == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// the `search` tool: a query run against the endpoints, or one page fetched - lines past `offset + PAGE_LINES` wait for the next call
pub fn run(policy: &Policy, query: Option<&str>, url: Option<&str>, endpoint: Option<&str>, offset: usize) -> Result<String, String> {
    let (title, lines) = match (url.map(str::trim).filter(|u| !u.is_empty()), query.map(str::trim).filter(|q| !q.is_empty())) {
        (Some(url), _) => {
            let page = policy.fetch(policy.check(url)?)?;
            let mut lines = vec![about(&page)];
            lines.extend(readable(&page)?);
            (format!("# {}", page.url), lines)
        }
        (None, Some(query)) => (format!("# search \"{query}\""), search_lines(policy, query, endpoint)?),
        (None, None) => return Err("pass `query` to search, or `url` to fetch one https page".into()),
    };
    let total = lines.len();
    let start = offset.min(total);
    let end = (start + PAGE_LINES).min(total);
    let mut out = format!("{title}\nrules: {}\n", policy.describe());
    if start > 0 || end < total {
        let more = if end < total { format!(", pass offset={end} for the next page") } else { String::new() };
        out.push_str(&format!("(showing lines {}-{end} of {total}{more})\n", start + 1));
    }
    out.push('\n');
    out.push_str(&lines[start..end].join("\n"));
    out.push('\n');
    Ok(out)
}

// the status line every fetched page opens with
fn about(page: &Fetched) -> String {
    let ct = if page.content_type.is_empty() { "no content type" } else { &page.content_type };
    match &page.from {
        Some(from) => format!("status {} - {ct} - redirected from {from}", page.status),
        None => format!("status {} - {ct}", page.status),
    }
}

// every endpoint's results for one query, each under its own heading
fn search_lines(policy: &Policy, query: &str, only: Option<&str>) -> Result<Vec<String>, String> {
    if policy.endpoints.is_empty() {
        return Err("no search endpoints are set: add one with `{query}` where the words go - the \
                    `ccc.search.endpoints` setting in VS Code, or `ccc run --search-endpoint \
                    'https://wiki.example/search?q={query}'` - or pass `url` to fetch a page"
            .into());
    }
    let chosen: Vec<&String> = policy
        .endpoints
        .iter()
        .filter(|e| only.is_none_or(|o| e.to_ascii_lowercase().contains(&o.to_ascii_lowercase())))
        .collect();
    if chosen.is_empty() {
        return Err(format!("no search endpoint matches '{}' - the endpoints are: {}", only.unwrap_or_default(), policy.endpoints.join(", ")));
    }
    let mut lines = Vec::new();
    let mut answered = 0;
    for template in chosen {
        let target = template.replace("{query}", &encode(query));
        lines.push(format!("## {target}"));
        match policy.check(&target).and_then(|u| policy.fetch(u)).and_then(|p| Ok((about(&p), readable(&p)?))) {
            Ok((status, text)) => {
                answered += 1;
                lines.push(status);
                let cut = text.len().saturating_sub(ENDPOINT_LINES);
                lines.extend(text.into_iter().take(ENDPOINT_LINES));
                if cut > 0 {
                    lines.push(format!("({cut} more line(s) - fetch the page with `url` for all of it)"));
                }
            }
            Err(e) => lines.push(e),
        }
        lines.push(String::new());
    }
    if answered == 0 {
        return Err(lines.into_iter().filter(|l| !l.is_empty() && !l.starts_with("## ")).collect::<Vec<_>>().join("\n"));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: Mode, urls: &[&str]) -> Policy {
        let urls: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
        Policy::new(mode, &urls, &[]).unwrap()
    }

    #[test]
    fn only_https_gets_through_whatever_the_mode() {
        let open = policy(Mode::None, &[]);
        assert!(open.check("https://example.com/a").is_ok());
        for bad in ["http://example.com", "ftp://example.com", "file:///etc/passwd", "example.com", "https://user:pw@example.com/"] {
            let err = open.check(bad).unwrap_err();
            assert!(err.starts_with(REFUSED), "{bad}: {err}");
        }
    }

    #[test]
    fn allow_lets_through_only_what_an_entry_matches() {
        let p = policy(Mode::Allow, &["https://docs.corp/**", "*.wiki.corp"]);
        assert!(p.check("https://docs.corp/billing/retries?x=1").is_ok());
        assert!(p.check("https://team.wiki.corp/page").is_ok());
        assert!(p.check("https://DOCS.CORP/x").is_ok());
        let err = p.check("https://example.com/").unwrap_err();
        assert!(err.contains("not on the allow list"), "{err}");
        // a host entry never matches a lookalike that merely contains it
        assert!(p.check("https://wiki.corp.evil.example/").is_err());
    }

    #[test]
    fn block_refuses_what_an_entry_matches_and_names_it() {
        let p = policy(Mode::Block, &["*.social.example", "https://example.com/private/**"]);
        assert!(p.check("https://example.com/public").is_ok());
        let err = p.check("https://example.com/private/key").unwrap_err();
        assert!(err.contains("https://example.com/private/**"), "{err}");
        assert!(p.check("https://feed.social.example/").is_err());
    }

    #[test]
    fn a_bad_entry_or_endpoint_is_an_error_up_front() {
        assert!(Policy::new(Mode::Allow, &["docs.corp/path".into()], &[]).is_err());
        assert!(Policy::new(Mode::None, &[], &["https://wiki.corp/search".into()]).is_err());
        assert!(Policy::new(Mode::None, &[], &["http://wiki.corp/search?q={query}".into()]).is_err());
        assert!(Policy::new(Mode::None, &[], &["https://wiki.corp/search?q={query}".into()]).is_ok());
        assert!(Mode::parse("deny").is_err());
    }

    #[test]
    fn a_redirect_resolves_against_the_page_it_came_from() {
        let base = parse_url("https://docs.corp/a/b/page?x=1").unwrap();
        assert_eq!(resolve(&base, "/root"), "https://docs.corp/root");
        assert_eq!(resolve(&base, "next"), "https://docs.corp/a/b/next");
        assert_eq!(resolve(&base, "//cdn.corp/x"), "https://cdn.corp/x");
        assert_eq!(resolve(&base, "https://other.corp/"), "https://other.corp/");
    }

    #[test]
    fn a_query_is_percent_encoded() {
        assert_eq!(encode("retry policy & co"), "retry%20policy%20%26%20co");
    }

    #[test]
    fn a_response_skips_the_proxy_and_continue_heads() {
        let raw = b"HTTP/1.1 200 Connection established\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/2 301\r\nLocation: /next\r\nContent-Type: text/html\r\n\r\nbody";
        let (status, headers, body) = split_response(raw).unwrap();
        assert_eq!(status, 301);
        assert!(headers.contains(&("location".to_string(), "/next".to_string())));
        assert_eq!(body, b"body");
    }

    #[test]
    fn html_reads_as_text_with_its_links() {
        let base = parse_url("https://wiki.corp/search?q=x").unwrap();
        let html = "<html><head><title>Results &amp; more</title><style>p{}</style></head><body>\
                    <script>var a = '<p>no</p>';</script><!-- hidden -->\
                    <h1>Retry   policy</h1><p>Use <a href=\"/doc/retries\">the retries doc</a> first.</p>\
                    <ul><li>one</li><li>two&nbsp;&#x41;</li></ul><a href=\"#top\">top</a></body></html>";
        let text = html_text(html, &base);
        assert!(text.starts_with("# Results & more"), "{text}");
        assert!(text.contains("Retry policy"), "{text}");
        assert!(text.contains("Use [the retries doc](https://wiki.corp/doc/retries) first."), "{text}");
        assert!(text.contains("- one") && text.contains("- two A"), "{text}");
        assert!(!text.contains("var a") && !text.contains("hidden") && !text.contains("p{}"), "{text}");
        assert!(!text.contains("(#top)"), "{text}");
    }

    #[test]
    fn nothing_to_do_and_no_endpoints_say_what_to_set() {
        let p = policy(Mode::None, &[]);
        assert!(run(&p, None, None, None, 0).unwrap_err().contains("pass `query`"));
        assert!(run(&p, Some("retries"), None, None, 0).unwrap_err().contains("ccc.search.endpoints"));
    }

    #[test]
    fn a_refused_url_is_never_fetched() {
        let p = policy(Mode::Allow, &["docs.corp"]);
        let err = run(&p, None, Some("https://example.com/"), None, 0).unwrap_err();
        assert!(err.starts_with(REFUSED), "{err}");
    }
}
