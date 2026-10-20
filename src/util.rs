//! Small helpers shared across the crate.

/// Error returned by [`path_match`] for a malformed pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPattern;

impl std::fmt::Display for BadPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("syntax error in pattern")
    }
}

impl std::error::Error for BadPattern {}

/// Reports whether name matches the shell pattern, with the same semantics
/// as Go's path.Match.
pub fn path_match(pattern: &str, name: &str) -> Result<bool, BadPattern> {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut p = &pattern[..];
    let mut n = &name[..];

    'pattern: while !p.is_empty() {
        let (star, chunk, rest) = scan_chunk(p);
        p = rest;
        if star && chunk.is_empty() {
            // Trailing * matches rest of string unless it has a /.
            return Ok(!n.contains(&'/'));
        }
        // Look for match at current position.
        let (t, ok) = match_chunk(chunk, n)?;
        // if we're the last chunk, make sure we've exhausted the name
        // otherwise we'll give a false result even if we could still match
        // using the star
        if ok && (t.is_empty() || !p.is_empty()) {
            n = t;
            continue;
        }
        if star {
            // Look for match skipping i+1 bytes.
            // Cannot skip /.
            let mut i = 0;
            while i < n.len() && n[i] != '/' {
                let (t, ok) = match_chunk(chunk, &n[i + 1..])?;
                if ok {
                    // if we're the last chunk, make sure we exhausted the name
                    if p.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    n = t;
                    continue 'pattern;
                }
                i += 1;
            }
        }
        // Before returning false with no error, check that the remainder of
        // the pattern is syntactically valid.
        while !p.is_empty() {
            let (_, chunk, rest) = scan_chunk(p);
            p = rest;
            match_chunk(chunk, &[])?;
        }
        return Ok(false);
    }
    Ok(n.is_empty())
}

fn scan_chunk(mut p: &[char]) -> (bool, &[char], &[char]) {
    let mut star = false;
    while let Some('*') = p.first() {
        p = &p[1..];
        star = true;
    }
    let mut inrange = false;
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => {
                // error check handled in match_chunk: bad pattern.
                if i + 1 < p.len() {
                    i += 1;
                }
            }
            '[' => inrange = true,
            ']' => inrange = false,
            '*' if !inrange => break,
            _ => {}
        }
        i += 1;
    }
    (star, &p[..i], &p[i..])
}

fn match_chunk<'a>(
    mut chunk: &[char],
    mut s: &'a [char],
) -> Result<(&'a [char], bool), BadPattern> {
    // failed records whether the match has failed. After the match fails,
    // the loop continues on processing chunk, checking that the pattern is
    // well-formed but no longer reading s.
    let mut failed = false;
    while !chunk.is_empty() {
        if !failed && s.is_empty() {
            failed = true;
        }
        match chunk[0] {
            '[' => {
                let mut r = '\0';
                if !failed {
                    r = s[0];
                    s = &s[1..];
                }
                chunk = &chunk[1..];
                let mut negated = false;
                if let Some('^') = chunk.first() {
                    negated = true;
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if !chunk.is_empty() && chunk[0] == ']' && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (lo, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    if chunk.first() == Some(&'-') {
                        let (h, rest) = get_esc(&chunk[1..])?;
                        hi = h;
                        chunk = rest;
                    }
                    if lo <= r && r <= hi {
                        matched = true;
                    }
                    nrange += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            '?' => {
                if !failed {
                    if s[0] == '/' {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
            '\\' => {
                chunk = &chunk[1..];
                if chunk.is_empty() {
                    return Err(BadPattern);
                }
                if !failed {
                    if chunk[0] != s[0] {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
            c => {
                if !failed {
                    if c != s[0] {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok((&[], false));
    }
    Ok((s, true))
}

fn get_esc(mut chunk: &[char]) -> Result<(char, &[char]), BadPattern> {
    if chunk.is_empty() || chunk[0] == '-' || chunk[0] == ']' {
        return Err(BadPattern);
    }
    if chunk[0] == '\\' {
        chunk = &chunk[1..];
        if chunk.is_empty() {
            return Err(BadPattern);
        }
    }
    let r = chunk[0];
    chunk = &chunk[1..];
    if chunk.is_empty() {
        return Err(BadPattern);
    }
    Ok((r, chunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test cases from Go's path package
    #[test]
    fn go_match_tests() {
        let cases: &[(&str, &str, Result<bool, BadPattern>)] = &[
            ("abc", "abc", Ok(true)),
            ("*", "abc", Ok(true)),
            ("*c", "abc", Ok(true)),
            ("a*", "a", Ok(true)),
            ("a*", "abc", Ok(true)),
            ("a*", "ab/c", Ok(false)),
            ("a*/b", "abc/b", Ok(true)),
            ("a*/b", "a/c/b", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxe/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxe/xxx/f", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/fff", Ok(false)),
            ("a*b?c*x", "abxbbxdbxebxczzx", Ok(true)),
            ("a*b?c*x", "abxbbxdbxebxczzy", Ok(false)),
            ("ab[c]", "abc", Ok(true)),
            ("ab[b-d]", "abc", Ok(true)),
            ("ab[e-g]", "abc", Ok(false)),
            ("ab[^c]", "abc", Ok(false)),
            ("ab[^b-d]", "abc", Ok(false)),
            ("ab[^e-g]", "abc", Ok(true)),
            ("a\\*b", "a*b", Ok(true)),
            ("a\\*b", "ab", Ok(false)),
            ("a?b", "a☺b", Ok(true)),
            ("a[^a]b", "a☺b", Ok(true)),
            ("a???b", "a☺b", Ok(false)),
            ("a[^a][^a][^a]b", "a☺b", Ok(false)),
            ("[a-ζ]*", "α", Ok(true)),
            ("*[a-ζ]", "A", Ok(false)),
            ("a?b", "a/b", Ok(false)),
            ("a*b", "a/b", Ok(false)),
            ("[\\]a]", "]", Ok(true)),
            ("[\\-]", "-", Ok(true)),
            ("[x\\-]", "x", Ok(true)),
            ("[x\\-]", "-", Ok(true)),
            ("[x\\-]", "z", Ok(false)),
            ("[\\-x]", "x", Ok(true)),
            ("[\\-x]", "-", Ok(true)),
            ("[\\-x]", "a", Ok(false)),
            ("[]a]", "]", Err(BadPattern)),
            ("[-]", "-", Err(BadPattern)),
            ("[x-]", "x", Err(BadPattern)),
            ("[x-]", "-", Err(BadPattern)),
            ("[x-]", "z", Err(BadPattern)),
            ("[-x]", "x", Err(BadPattern)),
            ("[-x]", "-", Err(BadPattern)),
            ("[-x]", "a", Err(BadPattern)),
            ("\\", "a", Err(BadPattern)),
            ("[a-b-c]", "a", Err(BadPattern)),
            ("[", "a", Err(BadPattern)),
            ("[^", "a", Err(BadPattern)),
            ("[^bc", "a", Err(BadPattern)),
            ("a[", "a", Err(BadPattern)),
            ("a[", "ab", Err(BadPattern)),
            ("a[", "x", Err(BadPattern)),
            ("a/b[", "x", Err(BadPattern)),
            ("*x", "xxx", Ok(true)),
        ];
        for (pattern, name, want) in cases {
            assert_eq!(
                path_match(pattern, name),
                *want,
                "path_match({pattern:?}, {name:?})"
            );
        }
    }
}
