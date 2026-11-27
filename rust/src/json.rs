//! JSON: a parser to a tree and string quoting. Strings are bytes: what an
//! LLM server sends is not trusted to be valid UTF-8.

use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u8>),
    Arr(Vec<Json>),
    Obj(Vec<(Vec<u8>, Json)>),
}

impl Json {
    /// A member of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn at(&self, i: usize) -> Option<&Json> {
        match self {
            Json::Arr(items) => items.get(i),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&[u8]> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn items(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => &[],
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Obj(_))
    }
}

const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, word: &[u8]) -> bool {
        if self.s[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.s.get(self.pos..self.pos + 4)?;
        let mut v = 0;
        for &d in digits {
            v = v * 16 + (d as char).to_digit(16)?;
        }
        self.pos += 4;
        Some(v)
    }

    /// A string literal; `pos` is at the opening quote.
    fn string(&mut self) -> Option<Vec<u8>> {
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = self.peek()?;
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            // a surrogate pair is one character
                            if (0xD800..0xDC00).contains(&cp) && self.s[self.pos..].starts_with(b"\\u") {
                                let save = self.pos;
                                self.pos += 2;
                                match self.hex4() {
                                    Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    }
                                    _ => self.pos = save,
                                }
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        other => out.push(other), // \" \\ \/
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn number(&mut self) -> Option<f64> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) {
            self.pos += 1;
        }
        core::str::from_utf8(&self.s[start..self.pos]).ok()?.parse().ok()
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.peek()? == b']' {
                    self.pos += 1;
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip_ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Some(Json::Arr(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'{' => {
                self.pos += 1;
                let mut members = Vec::new();
                self.skip_ws();
                if self.peek()? == b'}' {
                    self.pos += 1;
                    return Some(Json::Obj(members));
                }
                loop {
                    self.skip_ws();
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let key = self.string()?;
                    self.skip_ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.pos += 1;
                    members.push((key, self.value(depth + 1)?));
                    self.skip_ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            return Some(Json::Obj(members));
                        }
                        _ => return None,
                    }
                }
            }
            b't' if self.eat(b"true") => Some(Json::Bool(true)),
            b'f' if self.eat(b"false") => Some(Json::Bool(false)),
            b'n' if self.eat(b"null") => Some(Json::Null),
            b'-' | b'0'..=b'9' => self.number().map(Json::Num),
            _ => None,
        }
    }
}

/// Parses the first JSON value in `s` (leading whitespace allowed).
pub fn parse(s: &[u8]) -> Option<Json> {
    Parser { s, pos: 0 }.value(0)
}

/// Appends `s` as a JSON string literal.
pub fn quote(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(c >> 4) as usize]);
                out.push(HEX[(c & 15) as usize]);
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

/// Appends `"key":"value"`.
pub fn field(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    quote(out, key.as_bytes());
    out.push(b':');
    quote(out, value);
}
