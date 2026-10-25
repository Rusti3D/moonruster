// Emulation of python's configparser.ConfigParser(interpolation=None) with
// default options (delimiters '=' and ':', option names lower cased,
// strict mode, DEFAULT section support).

use indexmap::IndexMap;
use std::collections::HashSet;

pub type Section = IndexMap<String, String>;

#[derive(Debug, Clone)]
pub enum CpError {
    NoSection(String),
    NoOption(String, String),
    Parse(String),
}

impl std::fmt::Display for CpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CpError::NoSection(s) => write!(f, "No section: {}", crate::pyfmt::py_repr(&s.clone().into())),
            CpError::NoOption(o, s) => write!(
                f,
                "No option {} in section: {}",
                crate::pyfmt::py_repr(&o.clone().into()),
                crate::pyfmt::py_repr(&s.clone().into())
            ),
            CpError::Parse(m) => f.write_str(m),
        }
    }
}

pub const DEFAULT_SECTION: &str = "DEFAULT";

#[derive(Debug, Clone, Default)]
pub struct ConfigParser {
    pub defaults: Section,
    pub sections: IndexMap<String, Section>,
}

fn optionxform(s: &str) -> String {
    s.to_lowercase()
}

impl ConfigParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.defaults.clear();
        self.sections.clear();
    }

    pub fn sections(&self) -> Vec<String> {
        self.sections.keys().cloned().collect()
    }

    pub fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(section)
    }

    /// Python's `key in config`
    pub fn contains(&self, section: &str) -> bool {
        section == DEFAULT_SECTION || self.has_section(section)
    }

    pub fn has_option(&self, section: &str, option: &str) -> bool {
        let opt = optionxform(option);
        if section.is_empty() || section == DEFAULT_SECTION {
            return self.defaults.contains_key(&opt);
        }
        match self.sections.get(section) {
            None => false,
            Some(s) => s.contains_key(&opt) || self.defaults.contains_key(&opt),
        }
    }

    pub fn get(&self, section: &str, option: &str) -> Result<String, CpError> {
        let opt = optionxform(option);
        let sec = if section == DEFAULT_SECTION {
            None
        } else {
            Some(self.sections.get(section).ok_or_else(|| CpError::NoSection(section.to_string()))?)
        };
        if let Some(s) = sec {
            if let Some(v) = s.get(&opt) {
                return Ok(v.clone());
            }
        }
        self.defaults
            .get(&opt)
            .cloned()
            .ok_or_else(|| CpError::NoOption(opt.clone(), section.to_string()))
    }

    /// Items of a section merged with defaults (python's dict(config[section]))
    pub fn items(&self, section: &str) -> Result<Section, CpError> {
        if section == DEFAULT_SECTION {
            return Ok(self.defaults.clone());
        }
        let s = self.sections.get(section).ok_or_else(|| CpError::NoSection(section.to_string()))?;
        // python: section keys first, then default keys not in the section
        let mut out = s.clone();
        for (k, v) in &self.defaults {
            if !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        Ok(out)
    }

    pub fn add_section(&mut self, section: &str) -> Result<(), CpError> {
        if section == DEFAULT_SECTION {
            return Err(CpError::Parse(format!("Invalid section name: '{}'", section)));
        }
        if self.sections.contains_key(section) {
            return Err(CpError::Parse(format!("Section '{}' already exists", section)));
        }
        self.sections.insert(section.to_string(), Section::new());
        Ok(())
    }

    pub fn set(&mut self, section: &str, option: &str, value: &str) -> Result<(), CpError> {
        let opt = optionxform(option);
        if section.is_empty() || section == DEFAULT_SECTION {
            self.defaults.insert(opt, value.to_string());
            return Ok(());
        }
        let s = self.sections.get_mut(section).ok_or_else(|| CpError::NoSection(section.to_string()))?;
        s.insert(opt, value.to_string());
        Ok(())
    }

    pub fn remove_option(&mut self, section: &str, option: &str) -> bool {
        let opt = optionxform(option);
        if section.is_empty() || section == DEFAULT_SECTION {
            return self.defaults.shift_remove(&opt).is_some();
        }
        match self.sections.get_mut(section) {
            None => false,
            Some(s) => s.shift_remove(&opt).is_some(),
        }
    }

    /// Python's read_string()
    pub fn read_string(&mut self, data: &str, source: &str) -> Result<(), CpError> {
        let mut elements_added: HashSet<String> = HashSet::new();
        let mut elements_opt_added: HashSet<(String, String)> = HashSet::new();
        // cursect: None, Some(None) = defaults, Some(Some(name))
        let mut cursect: Option<Option<String>> = None;
        let mut sectname = String::new();
        let mut optname: Option<String> = None;
        let mut indent_level: usize = 0;
        // values being accumulated (multi-line)
        let mut pending: IndexMap<(Option<String>, String), Vec<String>> = IndexMap::new();
        let mut errors: Vec<(usize, String)> = Vec::new();
        for (idx, line) in data.lines().enumerate() {
            let lineno = idx + 1;
            let stripped = line.trim();
            let is_comment = stripped.starts_with('#') || stripped.starts_with(';');
            let value = if is_comment { "" } else { stripped };
            if value.is_empty() {
                // empty_lines_in_values=True: an empty line terminates nothing,
                // but python appends '' to the current value when inside one.
                if is_comment {
                    continue;
                }
                if let (Some(cs), Some(on)) = (&cursect, &optname) {
                    if let Some(v) = pending.get_mut(&(cs.clone(), on.clone())) {
                        v.push(String::new());
                    }
                }
                continue;
            }
            let cur_indent = line.len() - line.trim_start().len();
            if cursect.is_some() && optname.is_some() && cur_indent > indent_level {
                let key = (cursect.clone().unwrap(), optname.clone().unwrap());
                if let Some(v) = pending.get_mut(&key) {
                    v.push(value.to_string());
                }
                continue;
            }
            indent_level = cur_indent;
            if let Some(header) = match_sectcre(value) {
                if header == DEFAULT_SECTION {
                    cursect = Some(None);
                } else {
                    if self.sections.contains_key(&header) {
                        if elements_added.contains(&header) {
                            return Err(CpError::Parse(format!(
                                "While reading from '{}' [line {:2}]: section '{}' already exists",
                                source, lineno, header
                            )));
                        }
                    } else {
                        self.sections.insert(header.clone(), Section::new());
                    }
                    elements_added.insert(header.clone());
                    cursect = Some(Some(header.clone()));
                }
                sectname = header;
                optname = None;
            } else if cursect.is_none() {
                return Err(CpError::Parse(format!(
                    "File contains no section headers.\nfile: '{}', line: {}\n'{}\\n'",
                    source, lineno, line
                )));
            } else {
                match match_optcre(value) {
                    Some((name, val)) if !name.is_empty() => {
                        let oname = optionxform(name.trim_end());
                        let key = (sectname.clone(), oname.clone());
                        if elements_opt_added.contains(&key) {
                            return Err(CpError::Parse(format!(
                                "While reading from '{}' [line {:2}]: option '{}' in section '{}' already exists",
                                source, lineno, oname, sectname
                            )));
                        }
                        elements_opt_added.insert(key);
                        let cs = cursect.clone().unwrap();
                        pending.insert((cs, oname.clone()), vec![val.trim().to_string()]);
                        optname = Some(oname);
                    }
                    _ => {
                        errors.push((lineno, line.to_string()));
                    }
                }
            }
        }
        // join multiline values
        for ((sect, opt), vals) in pending {
            let joined = vals.join("\n").trim_end().to_string();
            match sect {
                None => {
                    self.defaults.insert(opt, joined);
                }
                Some(s) => {
                    self.sections.get_mut(&s).unwrap().insert(opt, joined);
                }
            }
        }
        if !errors.is_empty() {
            let mut msg = format!("Source contains parsing errors: '{}'", source);
            for (ln, l) in errors {
                msg.push_str(&format!("\n\t[line {:2}]: '{}\\n'", ln, l));
            }
            return Err(CpError::Parse(msg));
        }
        Ok(())
    }

    /// Python's ConfigParser.write()
    pub fn write(&self) -> String {
        let mut out = String::new();
        let write_section = |out: &mut String, name: &str, items: &Section| {
            out.push_str(&format!("[{}]\n", name));
            for (k, v) in items {
                out.push_str(&format!("{} = {}\n", k, v.replace('\n', "\n\t")));
            }
            out.push('\n');
        };
        if !self.defaults.is_empty() {
            write_section(&mut out, DEFAULT_SECTION, &self.defaults);
        }
        for (name, items) in &self.sections {
            write_section(&mut out, name, items);
        }
        out
    }

    /// Python's `{key: dict(val) for key, val in config.items()}`
    pub fn as_dict(&self) -> IndexMap<String, Section> {
        let mut out = IndexMap::new();
        out.insert(DEFAULT_SECTION.to_string(), self.defaults.clone());
        for name in self.sections.keys() {
            out.insert(name.clone(), self.items(name).unwrap());
        }
        out
    }
}

/// configparser.SECTCRE: r"\[(?P<header>.+)\]" applied with match()
fn match_sectcre(value: &str) -> Option<String> {
    if !value.starts_with('[') {
        return None;
    }
    let rest = &value[1..];
    // greedy .+ followed by ']' -> last ']' with at least one char before it
    let pos = rest.rfind(']')?;
    if pos == 0 {
        // ".+" requires at least one char; try to find a later ']'
        return None;
    }
    let header = &rest[..pos];
    if header.contains('\n') {
        return None;
    }
    Some(header.to_string())
}

/// configparser.OPTCRE: r"(?P<option>.*?)\s*(?P<vi>[=:])\s*(?P<value>.*)$"
fn match_optcre(value: &str) -> Option<(String, String)> {
    let pos = value.find(|c| c == '=' || c == ':')?;
    let name = value[..pos].trim_end().to_string();
    let val = value[pos + 1..].trim_start().to_string();
    Some((name, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CPython's configparser on the same texts
    /// (tools/gen_configparser_vectors.py): every mismatch is listed
    #[test]
    fn vectors() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../tests/data/configparser_vectors.json")).unwrap();
        let mut bad: Vec<String> = Vec::new();
        for case in v.as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let mut cp = ConfigParser::new();
            match (cp.read_string(text, "<cfg>"), case.get("error")) {
                (Err(e), Some(want)) => {
                    if e.to_string() != want.as_str().unwrap() {
                        bad.push(format!("{:?}: error {:?} != {:?}", text, e.to_string(), want));
                    }
                }
                (Err(e), None) => bad.push(format!("{:?}: error {:?}, python parsed it", text, e.to_string())),
                (Ok(()), Some(want)) => bad.push(format!("{:?}: parsed, python raised {:?}", text, want)),
                (Ok(()), None) => {
                    let items = serde_json::to_value(cp.as_dict()).unwrap();
                    if items != case["items"] {
                        bad.push(format!("{:?}: items {} != {}", text, items, case["items"]));
                    }
                    if serde_json::to_value(cp.sections()).unwrap() != case["sections"] {
                        bad.push(format!("{:?}: sections {:?} != {}", text, cp.sections(), case["sections"]));
                    }
                    if cp.write() != case["write"].as_str().unwrap() {
                        bad.push(format!("{:?}: write {:?} != {:?}", text, cp.write(), case["write"]));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
    }

    #[test]
    fn parse_basic() {
        let mut cp = ConfigParser::new();
        cp.read_string("[server]\nHost: 0.0.0.0\nport = 7125\n[foo bar]\nlist:\n  a\n  b\n", "x").unwrap();
        assert_eq!(cp.get("server", "host").unwrap(), "0.0.0.0");
        assert_eq!(cp.get("foo bar", "list").unwrap(), "\na\nb");
        assert!(cp.get("server", "missing").is_err());
        assert_eq!(
            cp.write(),
            "[server]\nhost = 0.0.0.0\nport = 7125\n\n[foo bar]\nlist = \n\ta\n\tb\n\n"
        );
    }
}
