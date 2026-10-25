// Python's shlex module, as Moonraker uses it (shlex.split)

/// Python's shlex.split(s): shlex.shlex(s, posix=True) with whitespace_split
/// and no commenters (CPython's read_token state machine). The error is the
/// ValueError's message
pub fn shlex_split(s: &str) -> Result<Vec<String>, String> {
    const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Quote(char),
        Escape,
    }
    let mut out = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    // the state an escape returns to: Word, or the double quote it is in
    let mut escaped = State::Word;
    let mut chars = s.chars();
    loop {
        let next = chars.next();
        match state {
            State::Space | State::Word => match next {
                None => break,
                Some(c) if WHITESPACE.contains(&c) => {
                    state = State::Space;
                    if !token.is_empty() || quoted {
                        out.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                }
                Some('\\') => {
                    escaped = State::Word;
                    state = State::Escape;
                }
                Some(q @ ('\'' | '"')) => state = State::Quote(q),
                Some(c) => {
                    token.push(c);
                    state = State::Word;
                }
            },
            State::Quote(q) => {
                quoted = true;
                match next {
                    None => return Err("No closing quotation".into()),
                    Some(c) if c == q => state = State::Word,
                    // only double quotes have escapes
                    Some('\\') if q == '"' => {
                        escaped = state;
                        state = State::Escape;
                    }
                    Some(c) => token.push(c),
                }
            }
            State::Escape => {
                let c = next.ok_or_else(|| "No escaped character".to_string())?;
                // in quotes, only the quote itself or the escape character
                // may be escaped
                if let State::Quote(q) = escaped {
                    if c != '\\' && c != q {
                        token.push('\\');
                    }
                }
                token.push(c);
                state = escaped;
            }
        }
    }
    if !token.is_empty() || quoted {
        out.push(token);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// CPython's shlex.split (tools/gen_misc_vectors.py)
    #[test]
    fn shlex_vectors() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../../tests/data/misc_vectors.json")).unwrap();
        let mut bad = Vec::new();
        for e in v["shlex"].as_array().unwrap() {
            let got = match super::shlex_split(e[0].as_str().unwrap()) {
                Ok(v) => serde_json::to_value(v).unwrap(),
                Err(msg) => serde_json::json!({ "error": msg }),
            };
            if got != e[1] {
                bad.push(format!("{:?}: {} != {}", e[0], got, e[1]));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
    }

    #[test]
    fn shlex() {
        assert_eq!(super::shlex_split("a 'b c' \"d \\\"e\" f\\ g").unwrap(), vec!["a", "b c", "d \"e", "f g"]);
    }
}
