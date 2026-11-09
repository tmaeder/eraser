//! Byte-exact port of the subset of Go's `net/url` used by the parser
//! (`url.Parse`, `(*URL).String`, `(*URL).Query().Get`).
//!
//! The `url` crate implements WHATWG URL semantics (lowercased hosts, IDNA,
//! trailing `/` on empty paths, dot-segment removal), which changes the URLs
//! we extract. Go's parser keeps them nearly verbatim, so we mirror it here.
//! Semantics follow Go 1.27 with the `go 1.23` module defaults
//! (`urlstrictcolons=0`).

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Path,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

fn should_escape(c: u8, mode: Mode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if matches!(mode, Mode::Host | Mode::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Mode::Path => return c == b'?',
            Mode::UserPassword => return matches!(c, b'@' | b'/' | b'?' | b':'),
            Mode::QueryComponent => return true,
            Mode::Fragment => return false,
            Mode::Host | Mode::Zone => {}
        },
        _ => {}
    }
    if mode == Mode::Fragment && matches!(c, b'!' | b'(' | b')' | b'*') {
        return false;
    }
    true
}

fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

fn unescape(s: &[u8], mode: Mode) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                if i + 2 >= s.len()
                    || !s[i + 1].is_ascii_hexdigit()
                    || !s[i + 2].is_ascii_hexdigit()
                {
                    return None;
                }
                if mode == Mode::Host && unhex(s[i + 1]) < 8 && &s[i..i + 3] != b"%25" {
                    return None;
                }
                if mode == Mode::Zone {
                    let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                    if &s[i..i + 3] != b"%25" && v != b' ' && should_escape(v, Mode::Host) {
                        return None;
                    }
                }
                i += 3;
            }
            c => {
                if matches!(mode, Mode::Host | Mode::Zone)
                    && c < 0x80
                    && c != b'+'
                    && should_escape(c, mode)
                {
                    return None;
                }
                i += 1;
            }
        }
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
                i += 2;
            }
            b'+' if mode == Mode::QueryComponent => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    Some(out)
}

fn escape(s: &[u8], mode: Mode) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if c == b' ' && mode == Mode::QueryComponent {
            out.push(b'+');
        } else if should_escape(c, mode) {
            out.extend_from_slice(&[b'%', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
        } else {
            out.push(c);
        }
    }
    out
}

fn valid_encoded(s: &[u8], mode: Mode) -> bool {
    s.iter().all(|&c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

fn valid_optional_port(port: &[u8]) -> bool {
    match port.split_first() {
        None => true,
        Some((b':', digits)) => digits.iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

fn valid_userinfo(s: &[u8]) -> bool {
    // Go iterates runes; any non-ASCII rune is invalid, so a byte check is equivalent.
    s.iter().all(|&c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'.'
                    | b'_'
                    | b':'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'%'
                    | b'@'
            )
    })
}

fn find(s: &[u8], c: u8) -> Option<usize> {
    s.iter().position(|&b| b == c)
}

fn rfind(s: &[u8], c: u8) -> Option<usize> {
    s.iter().rposition(|&b| b == c)
}

/// Equivalent of `netip.ParseAddr(s)` succeeding with a non-IPv4 (i.e. IPv6) result.
fn is_ipv6_literal(s: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(s) else {
        return false;
    };
    let addr = match s.split_once('%') {
        Some((_, "")) => return false,
        Some((a, _)) => a,
        None => s,
    };
    addr.parse::<std::net::Ipv6Addr>().is_ok()
}

fn parse_host(host: &[u8]) -> Option<Vec<u8>> {
    match rfind(host, b'[') {
        Some(i) if i > 0 => return None,
        Some(_) => {
            let close = rfind(host, b']')?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return None;
            }
            let colon_port = unescape(colon_port, Mode::Host)?;
            let hostname = &host[1..close];
            let mut unescaped = match hostname.windows(3).position(|w| w == b"%25") {
                Some(z) => {
                    let mut h = unescape(&hostname[..z], Mode::Host)?;
                    h.extend(unescape(&hostname[z..], Mode::Zone)?);
                    h
                }
                None => unescape(hostname, Mode::Host)?,
            };
            if !is_ipv6_literal(&unescaped) {
                return None;
            }
            unescaped.insert(0, b'[');
            unescaped.push(b']');
            unescaped.extend(colon_port);
            return Some(unescaped);
        }
        None => {}
    }
    // urlstrictcolons=0: the port starts at the last colon.
    if let Some(i) = rfind(host, b':')
        && !valid_optional_port(&host[i..])
    {
        return None;
    }
    unescape(host, Mode::Host)
}

type Userinfo = (Vec<u8>, Option<Vec<u8>>);

#[derive(Default)]
pub(crate) struct GoUrl {
    pub scheme: String,
    opaque: Vec<u8>,
    user: Option<Userinfo>,
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    raw_path: Vec<u8>,
    raw_query: Vec<u8>,
    fragment: Vec<u8>,
    raw_fragment: Vec<u8>,
    force_query: bool,
    omit_host: bool,
}

/// Go's `url.Parse`. Returns `None` wherever Go returns an error.
pub(crate) fn parse(raw: &str) -> Option<GoUrl> {
    let raw = raw.as_bytes();
    let (u, frag) = match find(raw, b'#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &b""[..]),
    };
    let mut url = parse_no_frag(u)?;
    if !frag.is_empty() {
        url.fragment = unescape(frag, Mode::Fragment)?;
        url.raw_fragment = if escape(&url.fragment, Mode::Fragment) == frag {
            Vec::new()
        } else {
            frag.to_vec()
        };
    }
    Some(url)
}

fn get_scheme(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i == 0 => return Some((b"", raw)),
            b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' if i == 0 => return None,
            b':' => return Some((&raw[..i], &raw[i + 1..])),
            _ => return Some((b"", raw)),
        }
    }
    Some((b"", raw))
}

fn parse_no_frag(raw: &[u8]) -> Option<GoUrl> {
    if raw.iter().any(|&b| b < b' ' || b == 0x7f) {
        return None;
    }
    let mut url = GoUrl::default();
    if raw == b"*" {
        url.path = b"*".to_vec();
        return Some(url);
    }
    let (scheme, mut rest) = get_scheme(raw)?;
    url.scheme = String::from_utf8_lossy(scheme).to_ascii_lowercase();

    if rest.ends_with(b"?") && rest.iter().filter(|&&b| b == b'?').count() == 1 {
        url.force_query = true;
        rest = &rest[..rest.len() - 1];
    } else if let Some(i) = find(rest, b'?') {
        url.raw_query = rest[i + 1..].to_vec();
        rest = &rest[..i];
    }

    if !rest.starts_with(b"/") {
        if !url.scheme.is_empty() {
            url.opaque = rest.to_vec();
            return Some(url);
        }
        let segment = &rest[..find(rest, b'/').unwrap_or(rest.len())];
        if segment.contains(&b':') {
            return None;
        }
    }

    if (!url.scheme.is_empty() || !rest.starts_with(b"///")) && rest.starts_with(b"//") {
        let mut authority = &rest[2..];
        rest = b"";
        if let Some(i) = find(authority, b'/') {
            rest = &authority[i..];
            authority = &authority[..i];
        }
        let at = rfind(authority, b'@');
        url.host = parse_host(at.map_or(authority, |i| &authority[i + 1..]))?;
        if let Some(i) = at {
            let userinfo = &authority[..i];
            if !valid_userinfo(userinfo) {
                return None;
            }
            url.user = Some(match find(userinfo, b':') {
                None => (unescape(userinfo, Mode::UserPassword)?, None),
                Some(c) => (
                    unescape(&userinfo[..c], Mode::UserPassword)?,
                    Some(unescape(&userinfo[c + 1..], Mode::UserPassword)?),
                ),
            });
        }
    } else if !url.scheme.is_empty() && rest.starts_with(b"/") {
        url.omit_host = true;
    }

    url.path = unescape(rest, Mode::Path)?;
    url.raw_path = if escape(&url.path, Mode::Path) == rest {
        Vec::new()
    } else {
        rest.to_vec()
    };
    Some(url)
}

impl GoUrl {
    fn escaped_path(&self) -> Vec<u8> {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Mode::Path)
            && unescape(&self.raw_path, Mode::Path).as_deref() == Some(&self.path[..])
        {
            return self.raw_path.clone();
        }
        if self.path == b"*" {
            return b"*".to_vec();
        }
        escape(&self.path, Mode::Path)
    }

    fn escaped_fragment(&self) -> Vec<u8> {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Mode::Fragment)
            && unescape(&self.raw_fragment, Mode::Fragment).as_deref() == Some(&self.fragment[..])
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Mode::Fragment)
    }

    /// Go's `(*URL).String`.
    pub(crate) fn to_go_string(&self) -> String {
        let mut buf: Vec<u8> = Vec::new();
        if !self.scheme.is_empty() {
            buf.extend_from_slice(self.scheme.as_bytes());
            buf.push(b':');
        }
        if !self.opaque.is_empty() {
            buf.extend_from_slice(&self.opaque);
        } else {
            if (!self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some())
                && !(self.omit_host && self.host.is_empty() && self.user.is_none())
            {
                if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                    buf.extend_from_slice(b"//");
                }
                if let Some((name, password)) = &self.user {
                    buf.extend(escape(name, Mode::UserPassword));
                    if let Some(p) = password {
                        buf.push(b':');
                        buf.extend(escape(p, Mode::UserPassword));
                    }
                    buf.push(b'@');
                }
                buf.extend(escape(&self.host, Mode::Host));
            }
            let mut path = &self.escaped_path()[..];
            let owned;
            if self.omit_host
                && self.host.is_empty()
                && self.user.is_none()
                && path.starts_with(b"//")
            {
                buf.extend_from_slice(b"%2F");
                owned = path[1..].to_vec();
                path = &owned;
            }
            if !path.is_empty() && path[0] != b'/' && !self.host.is_empty() {
                buf.push(b'/');
            }
            if buf.is_empty() && path[..find(path, b'/').unwrap_or(path.len())].contains(&b':') {
                buf.extend_from_slice(b"./");
            }
            buf.extend_from_slice(path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            buf.push(b'#');
            buf.extend(self.escaped_fragment());
        }
        // Everything except RawQuery is escaped to ASCII; RawQuery is a slice of the
        // (valid UTF-8) input, so this never actually loses data.
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Go's `u.Query().Get(key)`: first valid value for `key`, else empty.
    pub(crate) fn query_get(&self, key: &str) -> Vec<u8> {
        if self.raw_query.iter().filter(|&&b| b == b'&').count() + 1 > 10_000 {
            return Vec::new();
        }
        for pair in self.raw_query.split(|&b| b == b'&') {
            if pair.is_empty() || pair.contains(&b';') {
                continue;
            }
            let (k, v) = match find(pair, b'=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => (pair, &b""[..]),
            };
            let (Some(k), Some(v)) = (
                unescape(k, Mode::QueryComponent),
                unescape(v, Mode::QueryComponent),
            ) else {
                continue;
            };
            if k == key.as_bytes() {
                return v;
            }
        }
        Vec::new()
    }
}
