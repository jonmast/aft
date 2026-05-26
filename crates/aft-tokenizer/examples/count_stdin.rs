//! Token-counting helper for the description audit.
//!
//! Reads NDJSON from stdin (one `{ "label": "...", "text": "..." }` per
//! line) and writes NDJSON to stdout (`{ "label": "...", "tokens": N }`).
//! Pure dev tooling — not part of the published crate surface.

use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Tiny hand-rolled JSON read so we don't pull serde into the
        // dev-only binary.
        let (label, text) = parse_label_text(trimmed).expect("invalid input line");
        let tokens = aft_tokenizer::count_tokens(&text);
        writeln!(
            out,
            r#"{{"label":{},"tokens":{}}}"#,
            json_escape(&label),
            tokens
        )?;
    }
    Ok(())
}

fn parse_label_text(line: &str) -> Option<(String, String)> {
    let value: serde_json_lite::Value = serde_json_lite::from_str(line).ok()?;
    let obj = value.as_object()?;
    let label = obj.get("label")?.as_str()?.to_string();
    let text = obj.get("text")?.as_str()?.to_string();
    Some((label, text))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!(r"\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

mod serde_json_lite {
    //! Minimal JSON reader for `{ "label": "...", "text": "..." }` only.
    //! Skips anything more elaborate; sufficient for our NDJSON usage.

    use std::collections::HashMap;

    #[derive(Debug)]
    pub enum Value {
        String(String),
        Object(HashMap<String, Value>),
    }

    impl Value {
        pub fn as_object(&self) -> Option<&HashMap<String, Value>> {
            if let Value::Object(o) = self {
                Some(o)
            } else {
                None
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            if let Value::String(s) = self {
                Some(s)
            } else {
                None
            }
        }
    }

    pub fn from_str(input: &str) -> Result<Value, String> {
        let mut p = Parser {
            s: input.as_bytes(),
            i: 0,
        };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.i != p.s.len() {
            return Err(format!("trailing data at offset {}", p.i));
        }
        Ok(v)
    }

    struct Parser<'a> {
        s: &'a [u8],
        i: usize,
    }

    impl<'a> Parser<'a> {
        fn skip_ws(&mut self) {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            }
        }
        fn expect(&mut self, c: u8) -> Result<(), String> {
            if self.i < self.s.len() && self.s[self.i] == c {
                self.i += 1;
                Ok(())
            } else {
                Err(format!("expected {} at offset {}", c as char, self.i))
            }
        }
        fn value(&mut self) -> Result<Value, String> {
            self.skip_ws();
            if self.i >= self.s.len() {
                return Err("unexpected eof".into());
            }
            match self.s[self.i] {
                b'"' => Ok(Value::String(self.string()?)),
                b'{' => self.object(),
                other => Err(format!("unexpected byte {} at {}", other as char, self.i)),
            }
        }
        fn object(&mut self) -> Result<Value, String> {
            self.expect(b'{')?;
            self.skip_ws();
            let mut map = HashMap::new();
            if self.i < self.s.len() && self.s[self.i] == b'}' {
                self.i += 1;
                return Ok(Value::Object(map));
            }
            loop {
                self.skip_ws();
                let key = self.string()?;
                self.skip_ws();
                self.expect(b':')?;
                let val = self.value()?;
                map.insert(key, val);
                self.skip_ws();
                if self.i < self.s.len() && self.s[self.i] == b',' {
                    self.i += 1;
                    continue;
                }
                self.expect(b'}')?;
                break;
            }
            Ok(Value::Object(map))
        }
        fn string(&mut self) -> Result<String, String> {
            self.expect(b'"')?;
            let mut out = String::new();
            while self.i < self.s.len() {
                match self.s[self.i] {
                    b'"' => {
                        self.i += 1;
                        return Ok(out);
                    }
                    b'\\' => {
                        self.i += 1;
                        if self.i >= self.s.len() {
                            return Err("eof in escape".into());
                        }
                        match self.s[self.i] {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'b' => out.push('\u{0008}'),
                            b'f' => out.push('\u{000c}'),
                            b'u' => {
                                if self.i + 4 >= self.s.len() {
                                    return Err("short unicode escape".into());
                                }
                                let hex = std::str::from_utf8(&self.s[self.i + 1..self.i + 5])
                                    .map_err(|_| "non-utf8 unicode escape".to_string())?;
                                let code = u32::from_str_radix(hex, 16)
                                    .map_err(|_| "bad unicode escape".to_string())?;
                                self.i += 4;
                                if let Some(c) = char::from_u32(code) {
                                    out.push(c);
                                }
                            }
                            other => return Err(format!("bad escape \\{}", other as char)),
                        }
                        self.i += 1;
                    }
                    b => {
                        out.push(b as char);
                        self.i += 1;
                    }
                }
            }
            Err("unterminated string".into())
        }
    }
}
