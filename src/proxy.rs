//! Domain names (host names) served by a reverse proxy on this host, Caddy or nginx, and where
//! each one is forwarded to (upstream address:port, or static files / redirects).
//!
//! Caddy is asked for its live config through the admin API (localhost:2019), falling back to
//! /etc/caddy/Caddyfile. nginx has no such API, so /etc/nginx/nginx.conf is read with its
//! `include`s expanded, and each `server` block's `server_name`s and `*_pass` targets collected.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStats {
    /// "caddy" or "nginx".
    pub server: String,
    /// Whether a process of that server is running.
    pub running: bool,
    /// Where the domain list came from: "admin-api", "caddyfile" or "nginx.conf".
    pub source: String,
    pub domain_count: usize,
    pub domains: Vec<Site>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub name: String,
    /// Upstream `host:port`s it is proxied to, or "files", "redirect", "respond".
    pub targets: Vec<String>,
}

/// Which proxy to look at, from MTOP_PROXY: "auto" (default), "caddy", "nginx" or "off".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyMode {
    Auto,
    Caddy,
    Nginx,
    Off,
}

impl ProxyMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "caddy" => Ok(Self::Caddy),
            "nginx" => Ok(Self::Nginx),
            "off" | "none" | "0" | "false" => Ok(Self::Off),
            other => Err(format!("invalid MTOP_PROXY: {other} (auto, caddy, nginx or off)")),
        }
    }
}

const CADDYFILE: &str = "/etc/caddy/Caddyfile";
const NGINX_CONF: &str = "/etc/nginx/nginx.conf";
const CADDY_ADMIN: &str = "127.0.0.1:2019";

/// Domain → its targets.
type Sites = BTreeMap<String, BTreeSet<String>>;

pub fn collect(mode: ProxyMode) -> Option<ProxyStats> {
    match mode {
        ProxyMode::Off => None,
        ProxyMode::Caddy => Some(caddy()),
        ProxyMode::Nginx => Some(nginx()),
        ProxyMode::Auto => {
            let caddy_running = process_running("caddy");
            let nginx_running = process_running("nginx");
            if caddy_running || (!nginx_running && Path::new(CADDYFILE).exists()) {
                Some(caddy())
            } else if nginx_running || Path::new(NGINX_CONF).exists() {
                Some(nginx())
            } else {
                None
            }
        }
    }
}

fn stats(server: &str, source: &str, sites: Sites) -> ProxyStats {
    let domains: Vec<Site> = sites
        .into_iter()
        .map(|(name, mut targets)| {
            // An HTTP→HTTPS redirect block next to the real one isn't where the site goes.
            if targets.len() > 1 {
                targets.remove("redirect");
            }
            Site { name, targets: targets.into_iter().collect() }
        })
        .collect();
    ProxyStats {
        server: server.into(),
        running: process_running(server),
        source: source.into(),
        domain_count: domains.len(),
        domains,
    }
}

// ---------------------------------------------------------------------------
// Caddy
// ---------------------------------------------------------------------------

fn caddy() -> ProxyStats {
    if let Some(config) = caddy_admin_config() {
        let mut sites = Sites::new();
        caddy_json_sites(&config["apps"]["http"], &[], &mut sites);
        return stats("caddy", "admin-api", sites);
    }
    let sites = std::fs::read_to_string(CADDYFILE)
        .map(|s| caddyfile_sites(&s))
        .unwrap_or_default();
    stats("caddy", "caddyfile", sites)
}

/// GET /config/ from the admin API with a bare HTTP/1.0 request (no HTTP client dependency).
fn caddy_admin_config() -> Option<Value> {
    let addr = CADDY_ADMIN.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(b"GET /config/ HTTP/1.0\r\nHost: localhost:2019\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf = Vec::new();
    stream.take(16 * 1024 * 1024).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.") || head.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    serde_json::from_str(body).ok()
}

/// Walks the http app: a route's `host` matchers name the sites, and the handlers beneath it
/// (including nested subroutes) say where they go.
fn caddy_json_sites(v: &Value, hosts: &[String], out: &mut Sites) {
    match v {
        Value::Object(map) => {
            let mut scoped: Vec<String> = Vec::new();
            if let Some(Value::Array(matchers)) = map.get("match") {
                let mut set = BTreeSet::new();
                for m in matchers {
                    if let Some(Value::Array(hs)) = m.get("host") {
                        hs.iter().filter_map(Value::as_str).for_each(|h| add_host(&mut set, h));
                    }
                }
                scoped = set.into_iter().collect();
                for h in &scoped {
                    out.entry(h.clone()).or_default();
                }
            }
            let hosts: &[String] = if scoped.is_empty() { hosts } else { &scoped };
            let target = |t: String, out: &mut Sites| {
                for h in hosts {
                    out.entry(h.clone()).or_default().insert(t.clone());
                }
            };
            match map.get("handler").and_then(Value::as_str) {
                Some("reverse_proxy") => {
                    for u in map.get("upstreams").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(dial) = u.get("dial").and_then(Value::as_str) {
                            target(dial.to_string(), out);
                        }
                    }
                }
                Some("file_server") => target("files".into(), out),
                Some("static_response") => {
                    let redirect = map.get("headers").is_some_and(|h| h.get("Location").is_some());
                    target(if redirect { "redirect" } else { "respond" }.into(), out);
                }
                _ => {}
            }
            for child in map.values() {
                caddy_json_sites(child, hosts, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| caddy_json_sites(i, hosts, out)),
        _ => {}
    }
}

/// Sites of a Caddyfile: the addresses before each top-level `{` (skipping the global options
/// block and `(snippets)`), and the reverse_proxy / file_server / redir directives inside.
fn caddyfile_sites(src: &str) -> Sites {
    let mut out = Sites::new();
    let mut depth = 0usize;
    let mut pending: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in src.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut tokens: Vec<&str> = Vec::new();
        for t in line.split_whitespace() {
            match t.strip_suffix('{') {
                Some(rest) if !rest.is_empty() && !rest.ends_with('$') => tokens.extend([rest, "{"]),
                _ => tokens.push(t),
            }
        }
        if depth >= 1 && !current.is_empty() {
            let add = |t: String, out: &mut Sites| {
                for h in &current {
                    out.entry(h.clone()).or_default().insert(t.clone());
                }
            };
            match tokens.first().copied() {
                Some("reverse_proxy") | Some("php_fastcgi") => {
                    for arg in &tokens[1..] {
                        if *arg == "{" {
                            break;
                        }
                        if arg.starts_with('@') || arg.starts_with('/') || *arg == "*" {
                            continue;
                        }
                        add(upstream_addr(arg), &mut out);
                    }
                }
                Some("to") if depth >= 2 => {
                    tokens[1..].iter().for_each(|a| add(upstream_addr(a), &mut out));
                }
                Some("file_server") => add("files".into(), &mut out),
                Some("redir") => add("redirect".into(), &mut out),
                Some("respond") => add("respond".into(), &mut out),
                _ => {}
            }
        }
        for t in tokens {
            match t {
                "{" => {
                    if depth == 0 {
                        current.clear();
                        for addr in pending.drain(..) {
                            if !addr.starts_with('(') {
                                if let Some(h) = caddy_address_host(&addr) {
                                    out.entry(h.clone()).or_default();
                                    current.push(h);
                                }
                            }
                        }
                    }
                    depth += 1;
                }
                "}" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        current.clear();
                    }
                }
                t if depth == 0 => {
                    pending.extend(t.split(',').filter(|a| !a.is_empty()).map(String::from))
                }
                _ => {}
            }
        }
    }
    out
}

/// `http://localhost:8080` → `localhost:8080`.
fn upstream_addr(a: &str) -> String {
    let a = a.split_once("://").map_or(a, |(_, rest)| rest);
    a.trim_end_matches('/').to_string()
}

/// `https://example.com:443/path` → `example.com` (None for catch-alls, IPs and local names).
fn caddy_address_host(addr: &str) -> Option<String> {
    let a = addr.split_once("://").map_or(addr, |(_, rest)| rest);
    let a = a.split('/').next().unwrap_or("");
    let host = if a.starts_with('[') {
        a.split_once(']').map_or(a, |(h, _)| h).trim_start_matches('[')
    } else {
        a.rsplit_once(':').map_or(a, |(h, _)| h)
    };
    let mut set = BTreeSet::new();
    add_host(&mut set, host);
    set.pop_first()
}

// ---------------------------------------------------------------------------
// nginx
// ---------------------------------------------------------------------------

fn nginx() -> ProxyStats {
    let mut tokens = Vec::new();
    let mut seen = BTreeSet::new();
    nginx_tokens(Path::new(NGINX_CONF), &mut tokens, &mut seen, 0);
    stats("nginx", "nginx.conf", nginx_sites(&tokens))
}

/// Tokens of an nginx config file (words, `;`, `{`, `}`), with `include`d files spliced in.
fn nginx_tokens(path: &Path, out: &mut Vec<String>, seen: &mut BTreeSet<PathBuf>, level: u32) {
    if level > 8 || !seen.insert(path.to_path_buf()) {
        return;
    }
    let Ok(src) = std::fs::read_to_string(path) else { return };
    let tokens = tokenize_nginx(&src);
    let mut i = 0;
    while i < tokens.len() {
        let at_stmt_start = i == 0 || matches!(tokens[i - 1].as_str(), ";" | "{" | "}");
        if at_stmt_start && tokens[i] == "include" {
            i += 1;
            while i < tokens.len() && tokens[i] != ";" {
                let pattern = &tokens[i];
                let full = if pattern.starts_with('/') {
                    PathBuf::from(pattern)
                } else {
                    Path::new("/etc/nginx").join(pattern)
                };
                for f in glob(&full) {
                    nginx_tokens(&f, out, seen, level + 1);
                }
                i += 1;
            }
            i += 1; // the `;`
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
}

fn tokenize_nginx(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = src.chars().peekable();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '#' if cur.is_empty() => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' if cur.is_empty() => {
                for q in chars.by_ref() {
                    if q == c {
                        break;
                    }
                    cur.push(q);
                }
                out.push(std::mem::take(&mut cur));
            }
            ';' | '{' | '}' => {
                flush(&mut cur, &mut out);
                out.push(c.to_string());
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Statements as (words, opens-a-block), plus block ends as `None`.
fn nginx_statements(tokens: &[String]) -> Vec<Option<(Vec<&str>, bool)>> {
    let mut out = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    for t in tokens {
        match t.as_str() {
            ";" => out.push(Some((std::mem::take(&mut words), false))),
            "{" => out.push(Some((std::mem::take(&mut words), true))),
            "}" => {
                words.clear();
                out.push(None);
            }
            w => words.push(w),
        }
    }
    out
}

fn nginx_sites(tokens: &[String]) -> Sites {
    let stmts = nginx_statements(tokens);

    // First pass: `upstream name { server addr ...; }` blocks.
    let mut upstreams: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    for s in &stmts {
        match s {
            Some((w, true)) => {
                if w.first() == Some(&"upstream") {
                    if let Some(name) = w.get(1) {
                        upstreams.entry(name.to_string()).or_default();
                    }
                }
                stack.push(w.iter().take(2).copied().collect::<Vec<_>>().join(" "));
            }
            Some((w, false)) => {
                if let (Some(top), Some(&"server"), Some(addr)) = (stack.last(), w.first(), w.get(1)) {
                    if let Some(name) = top.strip_prefix("upstream ") {
                        upstreams.entry(name.to_string()).or_default().push(addr.to_string());
                    }
                }
            }
            None => {
                stack.pop();
            }
        }
    }

    // Second pass: `server { ... }` blocks inside `http`.
    let mut out = Sites::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let mut server_depth: Option<usize> = None;
    for s in &stmts {
        match s {
            Some((w, true)) => {
                let kw = w.first().copied().unwrap_or("");
                if kw == "server" && server_depth.is_none() && stack.last() == Some(&"http") {
                    server_depth = Some(stack.len());
                    names.clear();
                    targets.clear();
                }
                stack.push(kw);
            }
            Some((w, false)) if server_depth.is_some() => match w.first().copied() {
                Some("server_name") => {
                    let mut set = BTreeSet::new();
                    w[1..].iter().for_each(|n| add_host(&mut set, n));
                    names.extend(set);
                }
                Some("proxy_pass" | "fastcgi_pass" | "uwsgi_pass" | "scgi_pass" | "grpc_pass") => {
                    if let Some(url) = w.get(1) {
                        let addr = upstream_addr(url);
                        let host = addr.split('/').next().unwrap_or(&addr).to_string();
                        match upstreams.get(&host) {
                            Some(servers) if !servers.is_empty() => targets.extend(servers.iter().cloned()),
                            _ => {
                                targets.insert(host);
                            }
                        }
                    }
                }
                Some("return") if w.len() >= 3 && w[1].starts_with('3') => {
                    targets.insert("redirect".into());
                }
                Some("root") | Some("alias") => {
                    targets.insert("files".into());
                }
                _ => {}
            },
            Some(_) => {}
            None => {
                stack.pop();
                if server_depth == Some(stack.len()) {
                    server_depth = None;
                    for n in names.drain(..) {
                        out.entry(n).or_default().extend(targets.iter().cloned());
                    }
                }
            }
        }
    }
    out
}

/// Minimal glob: `*` in the file-name part only (what nginx configs use: `sites-enabled/*`,
/// `conf.d/*.conf`). Results are sorted, like nginx does.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let name = pattern.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !name.contains('*') {
        return vec![pattern.to_path_buf()];
    }
    let dir = pattern.parent().unwrap_or(Path::new("/"));
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| wildcard(name, n)))
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// Adds a host name, skipping catch-alls and local names that aren't real domains.
fn add_host(out: &mut BTreeSet<String>, host: &str) {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty()
        || h == "_"
        || h == "localhost"
        || h.starts_with('~')
        || h.starts_with('$')
        || h.starts_with('{')
        || h.parse::<std::net::IpAddr>().is_ok()
        || !h.contains('.')
    {
        return;
    }
    out.insert(h);
}

/// Whether any process's command name is `name` (from /proc/<pid>/comm).
fn process_running(name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    entries.filter_map(Result::ok).any(|e| {
        e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flat(sites: &Sites) -> Vec<(String, Vec<String>)> {
        sites.iter().map(|(k, v)| (k.clone(), v.iter().cloned().collect())).collect()
    }

    #[test]
    fn caddyfile_sites_and_targets() {
        let src = r#"
{
    email me@example.com
}
(common) {
    encode gzip
}
mtop-ovh.xf2.us {
    reverse_proxy localhost:8787
}
a.example.com, https://b.example.com:443 {
    import common
    handle /api/* {
        reverse_proxy @x http://10.0.0.5:3000 10.0.0.6:3000
    }
    handle {
        root * /srv/www
        file_server
    }
}
c.example.com {
    reverse_proxy {
        to 127.0.0.1:9000
    }
}
:8080 {
    respond "local"
}
"#;
        let sites = flat(&caddyfile_sites(src));
        let files_and_upstreams = vec!["10.0.0.5:3000".to_string(), "10.0.0.6:3000".into(), "files".into()];
        assert_eq!(
            sites,
            vec![
                ("a.example.com".to_string(), files_and_upstreams.clone()),
                ("b.example.com".to_string(), files_and_upstreams),
                ("c.example.com".to_string(), vec!["127.0.0.1:9000".to_string()]),
                ("mtop-ovh.xf2.us".to_string(), vec!["localhost:8787".to_string()]),
            ]
        );
    }

    #[test]
    fn caddy_json_sites_and_targets() {
        let cfg = json!({"servers": {"srv0": {"routes": [
            {"match": [{"host": ["a.example.com", "b.example.com"]}], "handle": [{"handler": "subroute",
              "routes": [
                {"match": [{"host": ["c.example.com"]}],
                 "handle": [{"handler": "reverse_proxy", "upstreams": [{"dial": "localhost:9000"}]}]},
                {"handle": [{"handler": "reverse_proxy", "upstreams": [{"dial": "10.0.0.2:80"}]}]}
              ]}]},
            {"match": [{"host": ["d.example.com"]}], "handle": [{"handler": "file_server"}]}
        ]}}});
        let mut out = Sites::new();
        caddy_json_sites(&cfg, &[], &mut out);
        assert_eq!(
            flat(&out),
            vec![
                ("a.example.com".to_string(), vec!["10.0.0.2:80".to_string()]),
                ("b.example.com".to_string(), vec!["10.0.0.2:80".to_string()]),
                ("c.example.com".to_string(), vec!["localhost:9000".to_string()]),
                ("d.example.com".to_string(), vec!["files".to_string()]),
            ]
        );
    }

    #[test]
    fn nginx_sites_and_targets() {
        let src = r#"
http {
    upstream app { server 127.0.0.1:3000; server 127.0.0.1:3001 weight=2; }
    server {
        listen 80 default_server;
        server_name _; # catch-all
        return 444;
    }
    server {
        listen 80;
        server_name proxy-one.md9.us www.md9.us;
        return 301 https://$host$request_uri;
    }
    server {
        listen 443 ssl;
        server_name proxy-one.md9.us;
        location / { proxy_pass http://127.0.0.1:8787; }
        location /app/ { proxy_pass http://app/; }
    }
    server {
        server_name "static.md9.us";
        root /var/www/html;
    }
}
"#;
        let sites = nginx_sites(&tokenize_nginx(src));
        let s = stats("nginx", "nginx.conf", sites);
        let got: Vec<(String, Vec<String>)> = s.domains.into_iter().map(|d| (d.name, d.targets)).collect();
        assert_eq!(
            got,
            vec![
                (
                    "proxy-one.md9.us".to_string(),
                    vec!["127.0.0.1:3000".to_string(), "127.0.0.1:3001".into(), "127.0.0.1:8787".into()]
                ),
                ("static.md9.us".to_string(), vec!["files".to_string()]),
                ("www.md9.us".to_string(), vec!["redirect".to_string()]),
            ]
        );
        assert_eq!(s.domain_count, 3);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*.conf", "a.conf"));
        assert!(!wildcard("*.conf", "a.conf.bak"));
        assert!(wildcard("*", "default"));
    }
}
