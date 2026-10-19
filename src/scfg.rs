//! Parser for the scfg configuration file format, compatible with
//! codeberg.org/emersion/go-scfg.

use std::fmt;

/// Limits the max block nesting depth to prevent stack overflows.
const MAX_NESTING_DEPTH: usize = 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directive {
    pub name: String,
    pub params: Vec<String>,
    /// None when the directive has no block, Some(empty) for an empty block.
    pub children: Option<Block>,
    pub lineno: usize,
}

pub type Block = Vec<Directive>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub fn parse(s: &str) -> Result<Block, Error> {
    let mut dec = Decoder {
        lines: s.lines(),
        lineno: 0,
        depth: 0,
    };
    let (block, closing) = dec.read_block()?;
    if closing {
        return Err(Error(format!("line {}: unexpected '}}'", dec.lineno)));
    }
    Ok(block)
}

struct Decoder<'a> {
    lines: std::str::Lines<'a>,
    lineno: usize,
    depth: usize,
}

impl Decoder<'_> {
    fn read_block(&mut self) -> Result<(Block, bool), Error> {
        self.depth += 1;
        let res = self.read_block_inner();
        self.depth -= 1;
        res
    }

    fn read_block_inner(&mut self) -> Result<(Block, bool), Error> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(Error("exceeded max block depth".to_owned()));
        }

        let mut block = Block::new();
        while let Some(l) = self.lines.next() {
            self.lineno += 1;
            let mut words =
                split_words(l).map_err(|e| Error(format!("line {}: {}", self.lineno, e)))?;
            if words.is_empty() {
                continue;
            }

            if words.len() == 1 && l.ends_with('}') {
                return Ok((block, true));
            }

            let d = if words.last().map(String::as_str) == Some("{") && l.ends_with('{') {
                words.pop();
                let (name, params) = if words.is_empty() {
                    (String::new(), Vec::new())
                } else {
                    let name = words.remove(0);
                    (name, words)
                };
                let start = self.lineno;
                let (children, closing) = self.read_block()?;
                if !closing {
                    return Err(Error(format!("line {start}: unterminated block")));
                }
                Directive {
                    name,
                    params,
                    children: Some(children),
                    lineno: self.lineno,
                }
            } else {
                let name = words.remove(0);
                Directive {
                    name,
                    params: words,
                    children: None,
                    lineno: self.lineno,
                }
            };
            block.push(d);
        }
        Ok((block, false))
    }
}

fn split_words(l: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut sb = String::new();
    let mut escape = false;
    let mut quote: Option<char> = None;
    let mut want_wsp = false;
    for ch in l.chars() {
        if escape {
            sb.push(ch);
            escape = false;
        } else if want_wsp && ch != ' ' && ch != '\t' {
            return Err("atom not allowed after quoted string".to_owned());
        } else if ch == '\\' {
            escape = true;
        } else if quote == Some(ch) {
            quote = None;
            want_wsp = true;
            if sb.is_empty() {
                words.push(String::new());
            }
        } else if quote.is_none() && words.is_empty() && sb.is_empty() && ch == '#' {
            return Ok(Vec::new());
        } else if quote.is_none() && (ch == '\'' || ch == '"') {
            if !sb.is_empty() {
                return Err("quoted string not allowed after atom".to_owned());
            }
            quote = Some(ch);
        } else if quote.is_none() && (ch == ' ' || ch == '\t') {
            if !sb.is_empty() {
                words.push(std::mem::take(&mut sb));
            }
            want_wsp = false;
        } else {
            sb.push(ch);
        }
    }
    if quote.is_some() {
        return Err("unterminated quoted string".to_owned());
    }
    if !sb.is_empty() {
        words.push(sb);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let b = parse(
            "# comment\nlisten ircs://\n\ntls \"cert file.pem\" key.pem\nblock a {\n  child x\n}\nempty {\n}\n",
        )
        .unwrap();
        assert_eq!(b.len(), 4);
        assert_eq!(b[0].name, "listen");
        assert_eq!(b[0].params, vec!["ircs://"]);
        assert_eq!(b[1].params, vec!["cert file.pem", "key.pem"]);
        assert_eq!(b[2].params, vec!["a"]);
        assert_eq!(b[2].children.as_ref().unwrap()[0].name, "child");
        assert_eq!(b[3].children, Some(vec![]));
    }

    #[test]
    fn quoting() {
        assert_eq!(
            split_words(r#"a "b c" 'd' "" e\ f"#).unwrap(),
            vec!["a", "b c", "d", "", "e f"]
        );
        assert!(split_words(r#"a "b"c"#).is_err());
        assert!(split_words(r#"a b"c""#).is_err());
        assert!(split_words(r#"a "b"#).is_err());
        assert_eq!(split_words("a #b").unwrap(), vec!["a", "#b"]);
    }

    #[test]
    fn errors() {
        assert!(parse("}\n").is_err());
        assert!(parse("a {\n").is_err());
    }
}
