// Python's os.path on POSIX (posixpath, genericpath): string operations,
// ported from CPython's source

/// os.path.normpath
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    // splitroot: '//' is kept, one or three-and-more slashes are '/'
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial.is_empty() && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let out = format!("{}{}", initial, comps.join("/"));
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

/// os.path.join(a, *p)
pub fn join<S: AsRef<str>>(a: &str, p: &[S]) -> String {
    let mut path = a.to_string();
    for b in p {
        let b = b.as_ref();
        if b.starts_with('/') {
            path = b.to_string();
        } else if path.is_empty() || path.ends_with('/') {
            path.push_str(b);
        } else {
            path.push('/');
            path.push_str(b);
        }
    }
    path
}

/// os.path.splitext: (root, ext); leading dots of the name are no extension
pub fn splitext(p: &str) -> (&str, &str) {
    let sep = p.rfind('/').map(|i| i as isize).unwrap_or(-1);
    if let Some(dot) = p.rfind('.') {
        if dot as isize > sep {
            let name = &p[(sep + 1) as usize..dot];
            if name.bytes().any(|c| c != b'.') {
                return (&p[..dot], &p[dot..]);
            }
        }
    }
    (p, "")
}

/// os.path.basename
pub fn basename(p: &str) -> &str {
    &p[p.rfind('/').map(|i| i + 1).unwrap_or(0)..]
}

/// os.path.dirname
pub fn dirname(p: &str) -> &str {
    let head = &p[..p.rfind('/').map(|i| i + 1).unwrap_or(0)];
    if !head.is_empty() && head.bytes().any(|c| c != b'/') {
        head.trim_end_matches('/')
    } else {
        head
    }
}

/// os.path.abspath (relative paths are joined to the current directory)
pub fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        return normpath(p);
    }
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
    normpath(&join(&cwd, &[p]))
}

/// os.path.relpath(path, start)
pub fn relpath(path: &str, start: &str) -> String {
    let (a, b) = (abspath(start), abspath(path));
    let start_list: Vec<&str> = a.split('/').filter(|x| !x.is_empty()).collect();
    let path_list: Vec<&str> = b.split('/').filter(|x| !x.is_empty()).collect();
    let i = start_list.iter().zip(&path_list).take_while(|(s, p)| s == p).count();
    let mut rel: Vec<&str> = vec![".."; start_list.len() - i];
    rel.extend(&path_list[i..]);
    match rel.split_first() {
        None => ".".into(),
        Some((first, rest)) => join(first, rest),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// CPython's posixpath (tools/gen_path_vectors.py)
    #[test]
    fn vectors() {
        let v: Value = serde_json::from_str(include_str!("../../tests/data/path_vectors.json")).unwrap();
        let s = |x: &Value| x.as_str().unwrap().to_string();
        let mut bad: Vec<String> = Vec::new();
        let mut check = |what: &str, input: &Value, got: Value, want: &Value| {
            if &got != want {
                bad.push(format!("{} {}: {} != {}", what, input, got, want));
            }
        };
        for e in v["normpath"].as_array().unwrap() {
            check("normpath", &e[0], Value::from(super::normpath(&s(&e[0]))), &e[1]);
        }
        for e in v["splitext"].as_array().unwrap() {
            let (r, x) = super::splitext(e[0].as_str().unwrap());
            check("splitext", &e[0], serde_json::json!([r, x]), &e[1]);
        }
        for e in v["basename"].as_array().unwrap() {
            check("basename", &e[0], Value::from(super::basename(e[0].as_str().unwrap())), &e[1]);
        }
        for e in v["dirname"].as_array().unwrap() {
            check("dirname", &e[0], Value::from(super::dirname(e[0].as_str().unwrap())), &e[1]);
        }
        for e in v["join"].as_array().unwrap() {
            let parts: Vec<String> = e[0].as_array().unwrap().iter().map(s).collect();
            check("join", &e[0], Value::from(super::join(&parts[0], &parts[1..])), &e[1]);
        }
        for e in v["relpath"].as_array().unwrap() {
            let (p, st) = (s(&e[0][0]), s(&e[0][1]));
            check("relpath", &e[0], Value::from(super::relpath(&p, &st)), &e[1]);
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }
}
