//! A stand-in forge: the API shape, served over loopback.
//!
//! The point is to exercise the REAL adapter and the real binary. A trait
//! double would prove the orchestration calls the methods it calls and nothing
//! about whether the adapter speaks the protocol -- and the protocol is where
//! the defects live. So this answers HTTP, and the test points a git remote at
//! it.
//!
//! It is also mutable from the test while a battery is running, which is what
//! makes "a commit landed mid-run" a thing that can actually be staged.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "stand-in-token";
pub const LOGIN: &str = "reaper-stand-in";

#[derive(Default)]
struct Request {
    title: String,
    head: String,
}

#[derive(Default)]
struct State {
    requests: BTreeMap<u64, Request>,
    /// Comments as the forge holds them: author login and body.
    comments: BTreeMap<u64, Vec<(String, String)>>,
    merged: Vec<u64>,
    /// Requests the forge will refuse to merge, and why.
    refuse_merge: BTreeMap<u64, String>,
    /// A head change applied just before the next single-request read answers.
    ///
    /// Deterministic where a timer is not: it lets a test put a new commit
    /// exactly between two steps of the loop instead of hoping a sleep lands
    /// in the right window.
    pending_head: BTreeMap<u64, String>,
}

pub struct StandIn {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl StandIn {
    pub fn start() -> StandIn {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local address");
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let st = Arc::clone(&shared);
                        std::thread::spawn(move || {
                            let _ = serve(s, &st);
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        StandIn { addr, state }
    }

    /// A remote URL naming a repository on this stand-in.
    pub fn remote(&self, owner: &str, name: &str) -> String {
        format!("http://{}/{owner}/{name}.git", self.addr)
    }

    /// The credential this stand-in accepts.
    pub fn token_value() -> &'static str {
        TOKEN
    }

    /// Scheme and host only, for the adapter's `url` key.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn add_request(&self, number: u64, title: &str, head: &str) {
        self.state.lock().unwrap().requests.insert(
            number,
            Request {
                title: title.to_string(),
                head: head.to_string(),
            },
        );
    }

    /// Stage a new commit on an open request, mid-battery if the test likes.
    pub fn set_head(&self, number: u64, head: &str) {
        if let Some(r) = self.state.lock().unwrap().requests.get_mut(&number) {
            r.head = head.to_string();
        }
    }

    /// Stage a new commit to appear on the next read of this request alone.
    pub fn set_head_on_next_read(&self, number: u64, head: &str) {
        self.state
            .lock()
            .unwrap()
            .pending_head
            .insert(number, head.to_string());
    }

    pub fn close_request(&self, number: u64) {
        self.state.lock().unwrap().requests.remove(&number);
    }

    /// Put a comment on a request as someone other than reaper.
    pub fn add_foreign_comment(&self, number: u64, login: &str, body: &str) {
        self.state
            .lock()
            .unwrap()
            .comments
            .entry(number)
            .or_default()
            .push((login.to_string(), body.to_string()));
    }

    /// Everything reaper's own identity has said on a request.
    pub fn own_comments(&self, number: u64) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .comments
            .get(&number)
            .map(|v| {
                v.iter()
                    .filter(|(who, _)| who == LOGIN)
                    .map(|(_, body)| body.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn merged(&self) -> Vec<u64> {
        self.state.lock().unwrap().merged.clone()
    }

    pub fn refuse_merge(&self, number: u64, why: &str) {
        self.state
            .lock()
            .unwrap()
            .refuse_merge
            .insert(number, why.to_string());
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn request_json(number: u64, r: &Request) -> String {
    format!(
        r#"{{"number":{number},"title":"{}","head":{{"sha":"{}"}}}}"#,
        json_escape(&r.title),
        json_escape(&r.head)
    )
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut bits = line.split_whitespace();
    let method = bits.next().unwrap_or("").to_string();
    let target = bits.next().unwrap_or("").to_string();

    let mut length = 0usize;
    let mut authorized = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            authorized = h.contains(TOKEN) || lower.contains("basic ");
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    let path = target.split('?').next().unwrap_or("").to_string();
    let (code, payload) = if !authorized {
        // The adapter must send a credential on every route. Answering 401
        // when it does not is how the test notices a missing header rather
        // than passing because the stand-in was lenient.
        (401, r#"{"message":"no credential"}"#.to_string())
    } else {
        route(&method, &path, &body, state)
    };

    let reason = match code {
        200 | 201 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Conflict",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )?;
    stream.flush()
}

fn route(method: &str, path: &str, body: &str, state: &Arc<Mutex<State>>) -> (u16, String) {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    // api/v1/user
    if method == "GET" && segments == ["api", "v1", "user"] {
        return (200, format!(r#"{{"login":"{LOGIN}"}}"#));
    }
    // api/v1/repos/{owner}/{name}/...
    if segments.len() >= 6 && segments[..3] == ["api", "v1", "repos"] {
        let tail = &segments[5..];
        let mut st = state.lock().unwrap();
        match (method, tail) {
            ("GET", ["pulls"]) => {
                let body = st
                    .requests
                    .iter()
                    .map(|(n, r)| request_json(*n, r))
                    .collect::<Vec<_>>()
                    .join(",");
                return (200, format!("[{body}]"));
            }
            ("GET", ["pulls", n]) => {
                let n: u64 = n.parse().unwrap_or(0);
                if let Some(next) = st.pending_head.remove(&n) {
                    if let Some(r) = st.requests.get_mut(&n) {
                        r.head = next;
                    }
                }
                return match st.requests.get(&n) {
                    Some(r) => (200, request_json(n, r)),
                    None => (404, r#"{"message":"no such request"}"#.to_string()),
                };
            }
            ("POST", ["pulls", n, "merge"]) => {
                let n: u64 = n.parse().unwrap_or(0);
                if let Some(why) = st.refuse_merge.get(&n).cloned() {
                    return (409, format!(r#"{{"message":"{}"}}"#, json_escape(&why)));
                }
                if !st.requests.contains_key(&n) {
                    return (404, r#"{"message":"no such request"}"#.to_string());
                }
                if !body.contains("rebase") {
                    // The verb matters: a merge that silently used a different
                    // method would produce a different history and no error.
                    return (
                        409,
                        r#"{"message":"this stand-in only accepts a rebase"}"#.to_string(),
                    );
                }
                st.merged.push(n);
                st.requests.remove(&n);
                return (200, "{}".to_string());
            }
            ("GET", ["issues", n, "comments"]) => {
                let n: u64 = n.parse().unwrap_or(0);
                let list = st
                    .comments
                    .get(&n)
                    .map(|v| {
                        v.iter()
                            .map(|(who, what)| {
                                format!(
                                    r#"{{"body":"{}","user":{{"login":"{}"}}}}"#,
                                    json_escape(what),
                                    json_escape(who)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                return (200, format!("[{list}]"));
            }
            ("POST", ["issues", n, "comments"]) => {
                let n: u64 = n.parse().unwrap_or(0);
                let text = extract_body_field(body).unwrap_or_default();
                st.comments
                    .entry(n)
                    .or_default()
                    .push((LOGIN.to_string(), text));
                return (201, "{}".to_string());
            }
            _ => {}
        }
    }
    (
        404,
        format!(r#"{{"message":"no route for {method} {path}"}}"#),
    )
}

/// The `body` field of a posted comment, undoing the escaping above.
fn extract_body_field(json: &str) -> Option<String> {
    let start = json.find("\"body\"")?;
    let rest = &json[start + 6..];
    let open = rest.find('"')?;
    let bytes: Vec<char> = rest[open + 1..].chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '"' => return Some(out),
            '\\' if i + 1 < bytes.len() => {
                match bytes[i + 1] {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' if i + 5 < bytes.len() => {
                        let hex: String = bytes[i + 2..i + 6].iter().collect();
                        if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(c);
                        }
                        i += 4;
                    }
                    other => out.push(other),
                }
                i += 1;
            }
            c => out.push(c),
        }
        i += 1;
    }
    Some(out)
}
