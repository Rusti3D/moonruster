// Python's inspect module: cleandoc

/// Python's inspect.cleandoc
pub fn cleandoc(s: &str) -> String {
    let s = s.trim();
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let margin = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = vec![lines[0].trim_start().to_string()];
    for l in &lines[1..] {
        if l.len() >= margin {
            out.push(l[margin..].to_string());
        } else {
            out.push(l.trim_start().to_string());
        }
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod cleandoc_tests {
    /// CPython's inspect.cleandoc (tools/gen_misc_vectors.py)
    #[test]
    fn vectors() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../../tests/data/misc_vectors.json")).unwrap();
        let mut bad = Vec::new();
        for e in v["cleandoc"].as_array().unwrap() {
            let got = super::cleandoc(e[0].as_str().unwrap());
            if got != e[1].as_str().unwrap() {
                bad.push(format!("{:?}: {:?} != {:?}", e[0], got, e[1]));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
    }
}
