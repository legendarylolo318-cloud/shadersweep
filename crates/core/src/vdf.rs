//! Minimal Valve KeyValues parsers: text (`.vdf`/`.acf`) and binary (`shortcuts.vdf`).

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VdfError {
    #[error("unexpected end of input")]
    Eof,
    #[error("syntax error at byte {0}: {1}")]
    Syntax(usize, &'static str),
    #[error("unknown binary VDF type 0x{0:02x}")]
    BadType(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(u64),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// Case-insensitive lookup of the first child with this key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(items) => items
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn children(&self) -> &[(String, Value)] {
        match self {
            Value::Obj(items) => items,
            _ => &[],
        }
    }

    /// Depth-first search for every value whose key matches (case-insensitive).
    pub fn find_all<'a>(&'a self, key: &str, out: &mut Vec<&'a Value>) {
        for (k, v) in self.children() {
            if k.eq_ignore_ascii_case(key) {
                out.push(v);
            }
            v.find_all(key, out);
        }
    }
}

// ---------------------------------------------------------------- text

enum Tok {
    Str(String),
    Open,
    Close,
    End,
    /// Unquoted platform conditional such as `[$WIN32]`; ignored.
    Cond,
}

struct Text<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Text<'_> {
    fn skip_ws(&mut self) {
        loop {
            while self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.b[self.pos..].starts_with(b"//") {
                while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Result<Tok, VdfError> {
        loop {
            match self.token()? {
                Tok::Cond => continue,
                t => return Ok(t),
            }
        }
    }

    fn token(&mut self) -> Result<Tok, VdfError> {
        self.skip_ws();
        let Some(&c) = self.b.get(self.pos) else {
            return Ok(Tok::End);
        };
        match c {
            b'{' => {
                self.pos += 1;
                Ok(Tok::Open)
            }
            b'}' => {
                self.pos += 1;
                Ok(Tok::Close)
            }
            b'"' => {
                self.pos += 1;
                let mut buf = Vec::new();
                loop {
                    let Some(&c) = self.b.get(self.pos) else {
                        return Err(VdfError::Eof);
                    };
                    self.pos += 1;
                    match c {
                        b'"' => break,
                        b'\\' => {
                            let Some(&e) = self.b.get(self.pos) else {
                                return Err(VdfError::Eof);
                            };
                            self.pos += 1;
                            match e {
                                b'n' => buf.push(b'\n'),
                                b't' => buf.push(b'\t'),
                                b'\\' => buf.push(b'\\'),
                                b'"' => buf.push(b'"'),
                                other => {
                                    buf.push(b'\\');
                                    buf.push(other);
                                }
                            }
                        }
                        other => buf.push(other),
                    }
                }
                Ok(Tok::Str(String::from_utf8_lossy(&buf).into_owned()))
            }
            _ => {
                let start = self.pos;
                while self.pos < self.b.len()
                    && !self.b[self.pos].is_ascii_whitespace()
                    && !matches!(self.b[self.pos], b'"' | b'{' | b'}')
                {
                    self.pos += 1;
                }
                let raw = &self.b[start..self.pos];
                if raw.len() >= 2 && raw[0] == b'[' && raw[raw.len() - 1] == b']' {
                    return Ok(Tok::Cond);
                }
                Ok(Tok::Str(String::from_utf8_lossy(raw).into_owned()))
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Vec<(String, Value)>, VdfError> {
        if depth > 64 {
            return Err(VdfError::Syntax(self.pos, "nesting too deep"));
        }
        let mut items = Vec::new();
        loop {
            match self.next()? {
                Tok::End if depth == 0 => return Ok(items),
                Tok::End => return Err(VdfError::Eof),
                Tok::Close if depth > 0 => return Ok(items),
                Tok::Close => return Err(VdfError::Syntax(self.pos, "unbalanced '}'")),
                Tok::Open => return Err(VdfError::Syntax(self.pos, "'{' without key")),
                Tok::Cond => continue,
                Tok::Str(key) => match self.next()? {
                    Tok::Str(v) => items.push((key, Value::Str(v))),
                    Tok::Open => items.push((key, Value::Obj(self.object(depth + 1)?))),
                    _ => return Err(VdfError::Syntax(self.pos, "key without value")),
                },
            }
        }
    }
}

/// Parse text VDF into a top-level object holding all root keys.
pub fn parse_text(input: &str) -> Result<Value, VdfError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut t = Text {
        b: input.as_bytes(),
        pos: 0,
    };
    Ok(Value::Obj(t.object(0)?))
}

// -------------------------------------------------------------- binary

struct Bin<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bin<'_> {
    fn u8(&mut self) -> Result<u8, VdfError> {
        let v = *self.b.get(self.pos).ok_or(VdfError::Eof)?;
        self.pos += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&[u8], VdfError> {
        let end = self.pos.checked_add(n).ok_or(VdfError::Eof)?;
        let s = self.b.get(self.pos..end).ok_or(VdfError::Eof)?;
        self.pos = end;
        Ok(s)
    }

    fn cstr(&mut self) -> Result<String, VdfError> {
        let rest = &self.b[self.pos.min(self.b.len())..];
        let n = rest.iter().position(|&c| c == 0).ok_or(VdfError::Eof)?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.pos += n + 1;
        Ok(s)
    }

    fn object(&mut self, depth: usize) -> Result<Vec<(String, Value)>, VdfError> {
        if depth > 64 {
            return Err(VdfError::Syntax(self.pos, "nesting too deep"));
        }
        let mut items = Vec::new();
        loop {
            if depth == 0 && self.pos >= self.b.len() {
                return Ok(items);
            }
            let t = self.u8()?;
            if t == 8 {
                return Ok(items);
            }
            let key = self.cstr()?;
            let v = match t {
                0 => Value::Obj(self.object(depth + 1)?),
                1 => Value::Str(self.cstr()?),
                2 | 3 | 4 | 6 => {
                    let b = self.take(4)?;
                    Value::Int(u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                }
                5 => {
                    // wide string, UTF-16 NUL terminated
                    loop {
                        let b = self.take(2)?;
                        if b == [0, 0] {
                            break;
                        }
                    }
                    Value::Str(String::new())
                }
                7 | 10 => {
                    let b = self.take(8)?;
                    let mut a = [0u8; 8];
                    a.copy_from_slice(b);
                    Value::Int(u64::from_le_bytes(a))
                }
                other => return Err(VdfError::BadType(other)),
            };
            items.push((key, v));
        }
    }
}

/// Parse binary VDF (e.g. `shortcuts.vdf`).
pub fn parse_binary(bytes: &[u8]) -> Result<Value, VdfError> {
    let mut r = Bin { b: bytes, pos: 0 };
    Ok(Value::Obj(r.object(0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_basic() {
        let v = parse_text(
            "// c\n\"AppState\"\n{\n\t\"appid\"\t\"105600\"\n\t\"name\" \"Ter\\\"raria\"\n\t\"Sub\" { \"k\" \"v\" }\n}\n",
        )
        .unwrap();
        let a = v.get("appstate").unwrap();
        assert_eq!(a.get("appid").unwrap().as_u64(), Some(105600));
        assert_eq!(a.get("name").unwrap().as_str(), Some("Ter\"raria"));
        assert_eq!(a.get("sub").unwrap().get("k").unwrap().as_str(), Some("v"));
    }

    #[test]
    fn text_crlf_conditionals_escapes() {
        let t = "\"A\"\r\n{\r\n\t\"path\"\t\t\"D:\\\\Games \\\"x\\\"\" [$WIN32]\r\n\t\"n\" { \"k\" \"v\" } [!$X360]\r\n\tbare value\r\n}\r\n";
        let v = parse_text(t).unwrap();
        let a = v.get("A").unwrap();
        assert_eq!(a.get("path").unwrap().as_str(), Some("D:\\Games \"x\""));
        assert_eq!(a.get("n").unwrap().get("k").unwrap().as_str(), Some("v"));
        assert_eq!(a.get("bare").unwrap().as_str(), Some("value"));
    }

    #[test]
    fn text_errors() {
        assert!(parse_text("\"a\" {").is_err());
        assert!(parse_text("}").is_err());
        assert!(parse_text("\"a\"").is_err());
    }

    #[test]
    fn binary_basic() {
        let mut d = Vec::new();
        d.extend(b"\x00shortcuts\x00\x000\x00");
        d.extend(b"\x01AppName\x00Foo\x00");
        d.extend(b"\x02appid\x00\x01\x00\x00\x80");
        d.extend(b"\x08\x08\x08");
        let v = parse_binary(&d).unwrap();
        let e = v.get("shortcuts").unwrap().get("0").unwrap();
        assert_eq!(e.get("appname").unwrap().as_str(), Some("Foo"));
        assert_eq!(e.get("appid").unwrap().as_u64(), Some(0x8000_0001));
    }

    #[test]
    fn binary_truncated() {
        assert!(parse_binary(b"\x00shortcuts\x00\x01a\x00b").is_err());
        assert_eq!(parse_binary(b"\x09x\x00"), Err(VdfError::BadType(9)));
    }
}

#[cfg(test)]
mod fuzz {
    use super::*;
    use proptest::prelude::*;

    fn quote(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\t' => o.push_str("\\t"),
                c => o.push(c),
            }
        }
        o.push('"');
        o
    }

    fn value() -> impl Strategy<Value = Value> {
        let leaf = any::<String>().prop_map(Value::Str);
        leaf.prop_recursive(5, 64, 6, |inner| {
            prop::collection::vec((any::<String>(), inner), 0..6).prop_map(Value::Obj)
        })
    }

    fn emit(v: &[(String, Value)], nl: &str, out: &mut String) {
        for (k, v) in v {
            out.push_str(&quote(k));
            match v {
                Value::Str(s) => {
                    out.push('\t');
                    out.push_str(&quote(s));
                    out.push_str(nl);
                }
                Value::Obj(items) => {
                    out.push_str(nl);
                    out.push('{');
                    out.push_str(nl);
                    emit(items, nl, out);
                    out.push('}');
                    out.push_str(nl);
                }
                Value::Int(_) => unreachable!(),
            }
        }
    }

    fn emit_bin(v: &[(String, Value)], out: &mut Vec<u8>) {
        for (k, v) in v {
            match v {
                Value::Str(s) => {
                    out.push(1);
                    out.extend(k.replace('\0', "").as_bytes());
                    out.push(0);
                    out.extend(s.replace('\0', "").as_bytes());
                    out.push(0);
                }
                Value::Obj(items) => {
                    out.push(0);
                    out.extend(k.replace('\0', "").as_bytes());
                    out.push(0);
                    emit_bin(items, out);
                    out.push(8);
                }
                Value::Int(n) => {
                    out.push(7);
                    out.extend(k.replace('\0', "").as_bytes());
                    out.push(0);
                    out.extend(n.to_le_bytes());
                }
            }
        }
    }

    fn strip_nul(v: &[(String, Value)]) -> Vec<(String, Value)> {
        v.iter()
            .map(|(k, v)| {
                let k = k.replace('\0', "");
                let v = match v {
                    Value::Str(s) => Value::Str(s.replace('\0', "")),
                    Value::Obj(i) => Value::Obj(strip_nul(i)),
                    Value::Int(n) => Value::Int(*n),
                };
                (k, v)
            })
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn text_never_panics(s in any::<String>()) {
            let _ = parse_text(&s);
        }

        #[test]
        fn text_never_panics_on_vdf_like(s in "[\"{}\\\\ \t\r\na-z/\\[\\]$]{0,200}") {
            let _ = parse_text(&s);
        }

        #[test]
        fn binary_never_panics(b in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_binary(&b);
        }

        #[test]
        fn binary_never_panics_on_typed_noise(
            b in prop::collection::vec(prop_oneof![0u8..=10, any::<u8>()], 0..512)
        ) {
            let _ = parse_binary(&b);
        }

        #[test]
        fn text_roundtrip(items in prop::collection::vec((any::<String>(), value()), 0..6), crlf in any::<bool>()) {
            let mut t = String::new();
            emit(&items, if crlf { "\r\n" } else { "\n" }, &mut t);
            prop_assert_eq!(parse_text(&t).unwrap(), Value::Obj(items));
        }

        #[test]
        fn binary_roundtrip(items in prop::collection::vec((any::<String>(), value()), 0..6)) {
            let items = strip_nul(&items);
            let mut b = Vec::new();
            emit_bin(&items, &mut b);
            prop_assert_eq!(parse_binary(&b).unwrap(), Value::Obj(items));
        }
    }
}
