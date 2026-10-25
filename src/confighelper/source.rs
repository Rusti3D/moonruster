// Config sources: the parsed files, their includes and backups

use super::*;

#[derive(Default, Clone)]
pub(super) struct SourceBackup {
    pub(super) raw_data: Vec<String>,
    pub(super) section_map: HashMap<String, Vec<usize>>,
    pub(super) option_map: HashMap<(String, String), Vec<usize>>,
    pub(super) config: String,
}

pub struct ConfigSource {
    pub config: RefCell<ConfigParser>,
    pub(super) is_file: bool,
    pub files: RefCell<Vec<PathBuf>>,
    pub(super) raw_config_data: RefCell<Vec<String>>,
    pub(super) updates_pending: RefCell<HashSet<usize>>,
    pub(super) file_section_map: RefCell<IndexMap<String, Vec<usize>>>,
    pub(super) file_option_map: RefCell<HashMap<(String, String), Vec<usize>>>,
    pub(super) save_lock: Cell<bool>,
    pub(super) backup: RefCell<Option<SourceBackup>>,
}

pub(super) fn section_re() -> &'static Regex {
    pub(super) static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[([^\]]+)\]").unwrap())
}

pub(super) fn expandtabs(line: &str, tabsize: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for c in line.chars() {
        if c == '\t' {
            let spaces = tabsize - (col % tabsize);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else if c == '\n' || c == '\r' {
            out.push(c);
            col = 0;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

/// python's str.splitlines()
pub fn splitlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                out.push(std::mem::take(&mut cur));
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub(super) fn first_opt_name(line: &str) -> String {
    line.split(|c| c == ':' || c == '=').next().unwrap_or("").trim().to_string()
}

#[derive(Default, Debug, Clone)]
pub(super) struct OptInfo {
    pub(super) indent: usize,
    pub(super) start: usize,
    pub(super) end: usize,
}

#[derive(Default, Debug, Clone)]
pub(super) struct SectionInfo {
    pub(super) indent: usize,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) options: IndexMap<String, OptInfo>,
}

impl ConfigSource {
    pub(super) fn base(is_file: bool) -> Self {
        ConfigSource {
            config: RefCell::new(ConfigParser::new()),
            is_file,
            files: RefCell::new(Vec::new()),
            raw_config_data: RefCell::new(Vec::new()),
            updates_pending: RefCell::new(HashSet::new()),
            file_section_map: RefCell::new(IndexMap::new()),
            file_option_map: RefCell::new(HashMap::new()),
            save_lock: Cell::new(false),
            backup: RefCell::new(None),
        }
    }

    pub fn new_file() -> Self {
        Self::base(true)
    }

    pub fn new_dict(cp: ConfigParser) -> Self {
        let s = Self::base(false);
        *s.config.borrow_mut() = cp;
        s
    }

    pub fn backup_source(&self) {
        *self.backup.borrow_mut() = Some(SourceBackup {
            raw_data: self.raw_config_data.borrow().clone(),
            section_map: self.file_section_map.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            option_map: self.file_option_map.borrow().clone(),
            config: self.config.borrow().write(),
        });
    }

    pub(super) fn acquire_save_lock(&self) -> CResult<()> {
        if !self.is_file || self.files.borrow().is_empty() {
            return Err(ConfigError("Can only modify file backed configurations".into()));
        }
        if self.save_lock.get() {
            return Err(ConfigError("Configuration locked, cannot modify".into()));
        }
        self.save_lock.set(true);
        Ok(())
    }

    pub fn set_option(&self, section: &str, option: &str, value: &str) -> CResult<()> {
        if !self.is_file {
            return self.config.borrow_mut().set(section, option, value).map_err(|e| ConfigError(e.to_string()));
        }
        self.acquire_save_lock()?;
        let res = self.set_option_inner(section, option, value);
        self.save_lock.set(false);
        res
    }

    pub(super) fn set_option_inner(&self, section: &str, option: &str, value: &str) -> CResult<()> {
        let value = value.trim();
        if let Ok(cur) = self.config.borrow().get(section, option) {
            if cur.trim() == value {
                return Ok(());
            }
        }
        let mut file_idx = 0usize;
        let (mut has_sec, mut has_opt) = (false, false);
        let key = (section.to_string(), option.to_string());
        if let Some(v) = self.file_option_map.borrow().get(&key) {
            file_idx = v[0];
            has_sec = true;
            has_opt = true;
        } else if let Some(v) = self.file_section_map.borrow().get(section) {
            file_idx = v[0];
            has_sec = true;
        }
        let mut buf = splitlines(&self.raw_config_data.borrow()[file_idx]);
        let mut new_opt_list = vec![format!("{}: {}", option, value)];
        if value.contains('\n') {
            new_opt_list = vec![format!("{}:", option)];
            new_opt_list.extend(value.split('\n').map(|v| format!("  {}", v)));
        }
        match self.find_section_info(section, &buf) {
            Some(sec_info) => {
                let mut indent = sec_info.indent;
                let mut opt_start = sec_info.end;
                let mut opt_end = sec_info.end;
                if let Some(oi) = sec_info.options.get(option) {
                    indent = oi.indent;
                    opt_start = oi.start;
                    opt_end = oi.end;
                } else if let Some((_, last)) = sec_info.options.last() {
                    indent = last.indent;
                }
                if indent > 0 {
                    let padding = " ".repeat(indent);
                    new_opt_list = new_opt_list.into_iter().map(|v| format!("{}{}", padding, v)).collect();
                }
                let tail: Vec<String> = buf[opt_end.min(buf.len())..].to_vec();
                buf.truncate(opt_start);
                buf.extend(new_opt_list);
                buf.extend(tail);
            }
            None => {
                new_opt_list.insert(0, format!("[{}]", section));
                if buf.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                    new_opt_list.insert(0, String::new());
                }
                buf.extend(new_opt_list);
            }
        }
        buf.push(String::new());
        let updated_cfg = buf.join("\n");
        let mut test = ConfigParser::new();
        let ok = test.read_string(&updated_cfg, "<string>").is_ok() && test.has_option(section, option);
        if !ok {
            return Err(ConfigError(format!(
                "Failed to set option '{}' in section [{}], file: {}",
                option,
                section,
                self.files.borrow()[file_idx].display()
            )));
        }
        self.raw_config_data.borrow_mut()[file_idx] = updated_cfg;
        self.updates_pending.borrow_mut().insert(file_idx);
        if !has_sec {
            self.file_section_map.borrow_mut().insert(section.to_string(), vec![file_idx]);
        }
        if !has_opt {
            self.file_option_map.borrow_mut().insert(key, vec![file_idx]);
        }
        let mut cfg = self.config.borrow_mut();
        if !cfg.has_section(section) {
            let _ = cfg.add_section(section);
        }
        let _ = cfg.set(section, option, value);
        Ok(())
    }

    pub fn remove_option(&self, section: &str, option: &str) -> CResult<()> {
        if !self.is_file {
            self.config.borrow_mut().remove_option(section, option);
            return Ok(());
        }
        self.acquire_save_lock()?;
        let res = (|| {
            let key = (section.to_string(), option.to_string());
            let indices = match self.file_option_map.borrow().get(&key) {
                None => return Ok(()),
                Some(v) => v.clone(),
            };
            let mut pending = Vec::new();
            for idx in indices {
                let mut buf = splitlines(&self.raw_config_data.borrow()[idx]);
                let err = || {
                    ConfigError(format!(
                        "Failed to remove option '{}' from section [{}], file: {}",
                        option,
                        section,
                        self.files.borrow()[idx].display()
                    ))
                };
                let sec_info = self.find_section_info(section, &buf).ok_or_else(err)?;
                let oi = sec_info.options.get(option).ok_or_else(err)?;
                let start = oi.start;
                let mut end = oi.end;
                if end < buf.len() && start > 0 && buf[start - 1].trim().is_empty() && buf[end].trim().is_empty() {
                    end += 1;
                }
                let tail: Vec<String> = buf[end..].to_vec();
                buf.truncate(start);
                buf.extend(tail);
                buf.push(String::new());
                let updated = buf.join("\n");
                let mut test = ConfigParser::new();
                if test.read_string(&updated, "<string>").is_err() || test.has_option(section, option) {
                    return Err(err());
                }
                pending.push((idx, updated));
            }
            for (idx, data) in pending {
                self.updates_pending.borrow_mut().insert(idx);
                self.raw_config_data.borrow_mut()[idx] = data;
            }
            self.file_option_map.borrow_mut().remove(&key);
            self.config.borrow_mut().remove_option(section, option);
            Ok(())
        })();
        self.save_lock.set(false);
        res
    }

    pub fn save(&self) -> bool {
        *self.backup.borrow_mut() = None;
        let pending: Vec<usize> = self.updates_pending.borrow().iter().cloned().collect();
        if pending.is_empty() {
            return false;
        }
        for idx in pending {
            let path = self.files.borrow()[idx].clone();
            if let Err(e) = std::fs::write(&path, &self.raw_config_data.borrow()[idx]) {
                log::error!("Failed to write config file {}: {}", path.display(), e);
            }
        }
        self.updates_pending.borrow_mut().clear();
        true
    }

    pub fn cancel(&self) -> CResult<()> {
        self.acquire_save_lock()?;
        let bkp = self.backup.borrow_mut().take();
        if let Some(b) = bkp {
            if !self.updates_pending.borrow().is_empty() {
                *self.raw_config_data.borrow_mut() = b.raw_data;
                *self.file_option_map.borrow_mut() = b.option_map;
                *self.file_section_map.borrow_mut() = b.section_map.into_iter().collect();
                let mut cp = ConfigParser::new();
                let _ = cp.read_string(&b.config, "<string>");
                *self.config.borrow_mut() = cp;
                self.updates_pending.borrow_mut().clear();
            }
        }
        self.save_lock.set(false);
        Ok(())
    }

    pub fn write_config(&self, dest_folder: &Path) -> std::io::Result<()> {
        let dest_folder = utils::resolve_path(&utils::expand_user(&dest_folder.to_string_lossy()));
        let files = self.files.borrow();
        let cfg_parent = files[0].parent().unwrap_or(Path::new("/")).to_path_buf();
        for (i, path) in files.iter().enumerate() {
            let dest_file = match path.strip_prefix(&cfg_parent) {
                Ok(rel) => dest_folder.join(rel),
                Err(_) => {
                    let pname = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    dest_folder.join(format!("{}-{}", pname, fname))
                }
            };
            if let Some(p) = dest_file.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&dest_file, &self.raw_config_data.borrow()[i])?;
        }
        Ok(())
    }

    pub(super) fn find_section_info(&self, section: &str, file_data: &[String]) -> Option<SectionInfo> {
        let mut result = SectionInfo::default();
        let mut found = false;
        let mut last_option = String::new();
        let mut opt_indent: i64 = -1;
        for (idx, line) in file_data.iter().enumerate() {
            let st = line.trim();
            if st.is_empty() || st.starts_with('#') || st.starts_with(';') {
                continue;
            }
            let line = expandtabs(line, 8);
            let line_indent = line.len() - line.trim().len();
            if opt_indent != -1 && line_indent as i64 > opt_indent {
                if !last_option.is_empty() {
                    if let Some(o) = result.options.get_mut(&last_option) {
                        o.end = idx + 1;
                    }
                }
                if found {
                    result.end = idx + 1;
                }
                continue;
            }
            if let Some(caps) = section_re().captures(&line) {
                opt_indent = -1;
                if found {
                    break;
                }
                let cursec = caps.get(1).unwrap().as_str();
                if section == cursec {
                    result.indent = line_indent;
                    result.start = idx;
                    result.end = idx + 1;
                    found = true;
                }
            } else {
                opt_indent = line_indent as i64;
                if found {
                    result.end = idx + 1;
                    last_option = first_opt_name(&line);
                    result.options.insert(last_option.clone(), OptInfo { indent: line_indent, start: idx, end: idx + 1 });
                }
            }
        }
        if found {
            Some(result)
        } else {
            None
        }
    }

    /// find_config_file(section): the file the section is (first) in
    pub fn find_config_file(&self, section: &str) -> Option<PathBuf> {
        let idx = *self.file_section_map.borrow().get(section)?.first()?;
        self.files.borrow().get(idx).cloned()
    }

    pub fn get_file_sections(&self) -> IndexMap<String, Vec<String>> {
        let files = self.files.borrow();
        let mut out: IndexMap<String, Vec<String>> =
            files.iter().map(|f| (f.to_string_lossy().to_string(), Vec::new())).collect();
        for (section, idxs) in self.file_section_map.borrow().iter() {
            for idx in idxs {
                let fname = files[*idx].to_string_lossy().to_string();
                out.entry(fname).or_default().push(section.clone());
            }
        }
        out
    }

    pub(super) fn write_buffer(&self, buffer: &[String], fpath: &Path) -> CResult<()> {
        if buffer.is_empty() {
            return Ok(());
        }
        let name = fpath.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.config
            .borrow_mut()
            .read_string(&buffer.join("\n"), &name)
            .map_err(|_| ConfigError(format!("Error Reading Config: '{}'", fpath.display())))
    }

    pub(super) fn parse_file(&self, file_path: &Path, visited: &mut Vec<(u64, u64)>) -> CResult<()> {
        use std::os::unix::fs::MetadataExt;
        let fail = |file_path: &Path| -> ConfigError {
            if !file_path.is_file() {
                return ConfigError(format!("Configuration File Not Found: '{}''", file_path.display()));
            }
            if !utils::access_read(file_path) {
                return ConfigError(format!(
                    "Moonraker does not have Read/Write permission for config file at path '{}'",
                    file_path.display()
                ));
            }
            ConfigError(format!("Error Reading Config: '{}'", file_path.display()))
        };
        let meta = std::fs::metadata(file_path).map_err(|_| fail(file_path))?;
        let cur_stat = (meta.dev(), meta.ino());
        if visited.contains(&cur_stat) {
            return Err(ConfigError(format!("Recursive include directive detected, {}", file_path.display())));
        }
        visited.push(cur_stat);
        self.files.borrow_mut().push(file_path.to_path_buf());
        let file_index = self.files.borrow().len() - 1;
        let cfg_data = match std::fs::read(file_path) {
            Ok(d) => String::from_utf8(d).map_err(|_| fail(file_path))?,
            Err(_) => return Err(fail(file_path)),
        };
        self.raw_config_data.borrow_mut().push(cfg_data.clone());
        let mut buffer: Vec<String> = Vec::new();
        let mut last_section = String::new();
        let mut opt_indent: i64 = -1;
        static CMT: OnceLock<Regex> = OnceLock::new();
        static UNESC: OnceLock<Regex> = OnceLock::new();
        let cmt_re = CMT.get_or_init(|| Regex::new(r" +[#;]").unwrap());
        let unesc_re = UNESC.get_or_init(|| Regex::new(r" \\(#|;)").unwrap());
        for raw_line in splitlines(&cfg_data) {
            let st = raw_line.trim_start();
            if raw_line.trim().is_empty() || st.starts_with('#') || st.starts_with(';') {
                continue;
            }
            let mut line = expandtabs(&raw_line, 4);
            if let Some(m) = cmt_re.find(&line) {
                line = line[..m.start()].to_string();
            }
            line = unesc_re.replace_all(&line, " $1").to_string();
            let line_indent = line.len() - line.trim_start().len();
            if opt_indent != -1 && line_indent as i64 > opt_indent {
                buffer.push(line);
                continue;
            }
            if let Some(caps) = section_re().captures(&line) {
                opt_indent = -1;
                let section = caps.get(1).unwrap().as_str().to_string();
                if let Some(inc) = section.strip_prefix("include ") {
                    let inc_path = inc.trim();
                    if inc_path.is_empty() {
                        return Err(ConfigError(format!("Invalid include directive: [{}]", section)));
                    }
                    let mut paths: Vec<PathBuf> = if inc_path.starts_with('/') {
                        let new_path = utils::resolve_path(Path::new(inc_path));
                        let parent = new_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                        let pat = new_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        utils::path_glob(&parent, &pat)
                    } else {
                        utils::path_glob(file_path.parent().unwrap_or(Path::new(".")), inc_path)
                    };
                    paths.sort();
                    if paths.is_empty() {
                        return Err(ConfigError(format!("No files matching include directive [{}]", section)));
                    }
                    self.write_buffer(&buffer, file_path)?;
                    buffer.clear();
                    for p in paths {
                        self.parse_file(&p, visited)?;
                    }
                    continue;
                } else {
                    last_section = section.clone();
                    let mut fsm = self.file_section_map.borrow_mut();
                    let entry = fsm.entry(section.clone()).or_default();
                    if entry.contains(&file_index) && !entry.is_empty() {
                        return Err(ConfigError(format!(
                            "Duplicate section [{}] in file {}",
                            section,
                            file_path.display()
                        )));
                    }
                    entry.insert(0, file_index);
                }
            } else {
                opt_indent = line_indent as i64;
                let option = first_opt_name(&line);
                let key = (last_section.clone(), option.clone());
                let mut fom = self.file_option_map.borrow_mut();
                let entry = fom.entry(key).or_default();
                if entry.contains(&file_index) {
                    return Err(ConfigError(format!(
                        "Duplicate option '{}' in section [{}], file {} ",
                        option,
                        last_section,
                        file_path.display()
                    )));
                }
                entry.insert(0, file_index);
            }
            buffer.push(line);
        }
        self.write_buffer(&buffer, file_path)
    }

    pub fn read_file(&self, main_conf: &Path) -> CResult<()> {
        self.config.borrow_mut().clear();
        self.files.borrow_mut().clear();
        self.raw_config_data.borrow_mut().clear();
        self.updates_pending.borrow_mut().clear();
        self.file_section_map.borrow_mut().clear();
        self.file_option_map.borrow_mut().clear();
        self.parse_file(main_conf, &mut Vec::new())?;
        let size: usize = self.raw_config_data.borrow().iter().map(|r| r.chars().count()).sum();
        log::info!("Configuration File '{}' parsed, total size: {} B", main_conf.display(), size);
        Ok(())
    }
}

pub fn get_configuration(config_file: &str, backup_config: Option<&str>, is_backup: bool) -> CResult<ConfigHelper> {
    let cfg_file = if is_backup { backup_config.unwrap_or(config_file) } else { config_file };
    let start_path = utils::absolute(&utils::expand_user(cfg_file));
    let source = Rc::new(ConfigSource::new_file());
    source.read_file(&start_path)?;
    if !source.config.borrow().has_section("server") {
        return Err(ConfigError("No section [server] in config".into()));
    }
    Ok(ConfigHelper::new(source, "server", Rc::new(RefCell::new(IndexMap::new())), None))
}

pub fn find_config_backup(cfg_path: &str) -> Option<String> {
    let cfg = utils::expand_user(cfg_path);
    let name = cfg.file_name()?.to_string_lossy().to_string();
    let backup = cfg.parent()?.join(format!(".{}.bkp", name));
    if backup.is_file() {
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    }
}
