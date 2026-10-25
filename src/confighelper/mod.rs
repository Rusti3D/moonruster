// Configuration Helper (port of moonraker/confighelper.py)

use crate::configparser::{ConfigParser, CpError};
use crate::server::Server;
use crate::utils;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

mod source;

pub use source::*;

pub const DOCS_URL: &str = "https://moonraker.readthedocs.io/en/latest";
pub const CFG_ERROR_KEY: &str = "__CONFIG_ERROR__";

#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for crate::common::ServerError {
    fn from(e: ConfigError) -> Self {
        crate::common::ServerError::new(e.0, 400)
    }
}

impl From<crate::common::ServerError> for ConfigError {
    fn from(e: crate::common::ServerError) -> Self {
        ConfigError(e.message)
    }
}

pub type CResult<T> = Result<T, ConfigError>;
type Parsed = Rc<RefCell<IndexMap<String, Map<String, Value>>>>;

enum FetchErr {
    Missing(String),
    Other(String),
}

impl From<CpError> for FetchErr {
    fn from(e: CpError) -> Self {
        match e {
            CpError::NoOption(..) | CpError::NoSection(..) => FetchErr::Missing(e.to_string()),
            CpError::Parse(m) => FetchErr::Other(m),
        }
    }
}

/// The value type of a get_py() option
#[derive(Clone, Copy)]
pub enum OptKind {
    Str,
    Int,
    Float,
    Bool,
}

/// What raw_get did not find (configparser's NoSectionError / NoOptionError)
pub enum RawMissing {
    Section,
    Option,
}

/// A numeric option's value as a number (and whether it is an int) and its
/// bounds
type NumericCheck<'a, T> = (&'a dyn Fn(&T) -> Option<(f64, bool)>, Bounds);

#[derive(Default, Clone, Copy)]
pub struct Bounds {
    pub above: Option<f64>,
    pub below: Option<f64>,
    pub minval: Option<f64>,
    pub maxval: Option<f64>,
}

impl Bounds {
    pub fn none() -> Self {
        Self::default()
    }
    pub fn min(v: f64) -> Self {
        Bounds { minval: Some(v), ..Default::default() }
    }
    pub fn above(v: f64) -> Self {
        Bounds { above: Some(v), ..Default::default() }
    }
    pub fn range(min: f64, max: f64) -> Self {
        Bounds { minval: Some(min), maxval: Some(max), ..Default::default() }
    }
}

fn fmt_num(v: f64, is_int: bool) -> String {
    if is_int {
        format!("{}", v as i64)
    } else {
        crate::pyfmt::py_float_repr(v)
    }
}

#[derive(Clone)]
pub struct ConfigHelper {
    pub source: Rc<ConfigSource>,
    pub section: String,
    pub fallback_section: Option<String>,
    parsed: Parsed,
}

impl ConfigHelper {
    fn new(source: Rc<ConfigSource>, section: &str, parsed: Parsed, fallback: Option<String>) -> Self {
        parsed.borrow_mut().entry(section.to_string()).or_default();
        ConfigHelper { source, section: section.to_string(), fallback_section: fallback, parsed }
    }

    pub fn error(msg: impl Into<String>) -> ConfigError {
        ConfigError(msg.into())
    }

    pub fn get_name(&self) -> &str {
        &self.section
    }

    pub fn sections(&self) -> Vec<String> {
        self.source.config.borrow().sections()
    }

    pub fn has_section(&self, section: &str) -> bool {
        self.source.config.borrow().has_section(section)
    }

    pub fn has_option(&self, option: &str) -> bool {
        self.source.config.borrow().has_option(&self.section, option)
    }

    pub fn set_option(&self, option: &str, value: &str) -> CResult<()> {
        self.source.set_option(&self.section, option, value)
    }

    pub fn get_options(&self) -> IndexMap<String, String> {
        let cfg = self.source.config.borrow();
        if !cfg.contains(&self.section) {
            return IndexMap::new();
        }
        cfg.items(&self.section).unwrap_or_default()
    }

    pub fn get_hash(&self) -> sha2::Sha256 {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        let cfg = self.source.config.borrow();
        if !cfg.contains(&self.section) {
            return hash;
        }
        for (opt, val) in cfg.items(&self.section).unwrap_or_default() {
            hash.update(opt.as_bytes());
            hash.update(val.as_bytes());
        }
        hash
    }

    pub fn get_prefix_sections(&self, prefix: &str) -> Vec<String> {
        self.sections().into_iter().filter(|s| s.starts_with(prefix)).collect()
    }

    pub fn getsection(&self, section: &str) -> ConfigHelper {
        ConfigHelper::new(self.source.clone(), section, self.parsed.clone(), None)
    }

    pub fn getsection_fallback(&self, section: &str, fallback: Option<&str>) -> ConfigHelper {
        ConfigHelper::new(self.source.clone(), section, self.parsed.clone(), fallback.map(String::from))
    }

    fn server(&self) -> Option<Rc<Server>> {
        crate::server::try_server()
    }

    fn add_warning(&self, msg: String) {
        match self.server() {
            Some(s) => {
                s.add_warning(&msg, None, true);
            }
            None => log::warn!("{}", msg),
        }
    }

    fn get_option<T>(
        &self,
        func: &dyn Fn(&ConfigParser, &str, &str) -> Result<T, FetchErr>,
        option: &str,
        default: Option<T>,
        numeric: Option<NumericCheck<'_, T>>,
        deprecate: bool,
        to_json: &dyn Fn(&T) -> Value,
    ) -> CResult<T> {
        let mut section = self.section.clone();
        let mut warn_fallback = false;
        {
            let cfg = self.source.config.borrow();
            if !cfg.contains(&self.section) {
                if let Some(fb) = &self.fallback_section {
                    section = fb.clone();
                    warn_fallback = true;
                }
            }
        }
        let res = {
            let cfg = self.source.config.borrow();
            func(&cfg, &section, option)
        };
        let val = match res {
            Err(FetchErr::Missing(msg)) => match default {
                None => {
                    self.mark_error();
                    return Err(ConfigError(msg));
                }
                Some(d) => {
                    section = self.section.clone();
                    d
                }
            },
            Err(FetchErr::Other(msg)) => {
                self.mark_error();
                return Err(ConfigError(format!(
                    "[{}]: Option '{}' encountered the following error while parsing: {}",
                    self.section, option, msg
                )));
            }
            Ok(v) => {
                if deprecate {
                    self.add_warning(format!(
                        "[{}]: Option '{}' is deprecated, see the configuration documentation at {}/configuration/",
                        self.section, option, DOCS_URL
                    ));
                }
                if warn_fallback {
                    let help = format!("{}/configuration/#option-moved-deprecations", DOCS_URL);
                    self.add_warning(format!(
                        "[{}]: Option '{}' has been moved to section [{}].  Please correct your configuration, see {} for detailed documentation.",
                        section, option, self.section, help
                    ));
                }
                if let Some((extract, bounds)) = numeric {
                    if let Some((num, is_int)) = extract(&v) {
                        self.check_option(option, num, is_int, bounds)?;
                    }
                }
                v
            }
        };
        let mut parsed = self.parsed.borrow_mut();
        let entry = parsed.entry(section).or_default();
        if !entry.contains_key(option) {
            entry.insert(option.to_string(), to_json(&val));
        }
        Ok(val)
    }

    /// configparser's get(section, option) on the parsed config, without
    /// recording (ConfigHelper.config for Python components)
    pub fn raw_get(&self, section: &str, option: &str) -> Result<String, RawMissing> {
        let cfg = self.source.config.borrow();
        if !cfg.contains(section) {
            return Err(RawMissing::Section);
        }
        cfg.get(section, option).map_err(|_| RawMissing::Option)
    }

    /// Moonraker's _get_option with the caller's parse function (Python
    /// components: getlists, getdict, gettemplate ... run as Moonraker's
    /// own code over this). `fetch(section, option)` gives Err(Ok(message))
    /// for a missing option or section, Err(Err(message)) for a parse error
    #[allow(clippy::type_complexity)]
    pub fn get_option_with<T>(
        &self,
        fetch: &dyn Fn(&str, &str) -> Result<T, Result<String, String>>,
        option: &str,
        default: Option<T>,
        numeric: Option<NumericCheck<'_, T>>,
        deprecate: bool,
        to_json: &dyn Fn(&T) -> Value,
    ) -> CResult<T> {
        let f = |_: &ConfigParser, s: &str, o: &str| -> Result<T, FetchErr> {
            fetch(s, o).map_err(|e| match e {
                Ok(m) => FetchErr::Missing(m),
                Err(m) => FetchErr::Other(m),
            })
        };
        self.get_option(&f, option, default, numeric, deprecate, to_json)
    }

    fn mark_error(&self) {
        self.parsed
            .borrow_mut()
            .entry(self.section.clone())
            .or_default()
            .insert(CFG_ERROR_KEY.into(), Value::Bool(true));
    }

    fn check_option(&self, option: &str, value: f64, is_int: bool, b: Bounds) -> CResult<()> {
        let vstr = fmt_num(value, is_int);
        let bstr = |x: f64| fmt_num(x, is_int && x.fract() == 0.0);
        if let Some(a) = b.above {
            if value <= a {
                return Err(ConfigError(format!(
                    "Config Error: Section [{}], Option '{}: {}': value is not above {}",
                    self.section, option, vstr, bstr(a)
                )));
            }
        }
        if let Some(a) = b.below {
            if value >= a {
                return Err(ConfigError(format!(
                    "Config Error: Section [{}], Option '{}: {}': value is not below {}",
                    self.section, option, vstr, bstr(a)
                )));
            }
        }
        if let Some(a) = b.minval {
            if value < a {
                return Err(ConfigError(format!(
                    "Config Error: Section [{}], Option '{}: {}': value is below minimum value {}",
                    self.section, option, vstr, bstr(a)
                )));
            }
        }
        if let Some(a) = b.maxval {
            if value > a {
                // Note: moonraker reports minval in this message
                let mv = b.minval.map(bstr).unwrap_or_else(|| "None".into());
                return Err(ConfigError(format!(
                    "Config Error: Section [{}], Option '{}: {}': value is above maximum value {}",
                    self.section, option, vstr, mv
                )));
            }
        }
        Ok(())
    }

    /// Moonraker's get / getint / getfloat / getboolean for Python components
    /// (plugins): the same parsing, errors, bounds, deprecation warning and
    /// recording as the getters below; `default` None is Sentinel.MISSING,
    /// else Python's default value, recorded as the parsed value
    pub fn get_py(&self, kind: OptKind, option: &str, default: Option<Value>, bounds: Bounds, deprecate: bool) -> CResult<Value> {
        let ex = |v: &Value| -> Option<(f64, bool)> {
            match (kind, v) {
                (OptKind::Int, Value::Number(n)) => n.as_f64().map(|f| (f, true)),
                (OptKind::Float, Value::Number(n)) => n.as_f64().map(|f| (f, false)),
                _ => None,
            }
        };
        let numeric = matches!(kind, OptKind::Int | OptKind::Float).then_some((&ex as &dyn Fn(&Value) -> Option<(f64, bool)>, bounds));
        let fetch = move |c: &ConfigParser, s: &str, o: &str| -> Result<Value, FetchErr> {
            Ok(match kind {
                OptKind::Str => Value::String(c.get(s, o)?),
                OptKind::Int => Value::from(Self::parse_int(c, s, o)?),
                OptKind::Float => utils::float_value(Self::parse_float(c, s, o)?),
                OptKind::Bool => Value::Bool(Self::parse_bool(c, s, o)?),
            })
        };
        self.get_option(&fetch, option, default, numeric, deprecate, &|v| v.clone())
    }

    // ***** String options *****

    pub fn get(&self, option: &str) -> CResult<String> {
        self.get_option(&|c, s, o| Ok(c.get(s, o)?), option, None, None, false, &|v| Value::String(v.clone()))
    }

    pub fn get_or(&self, option: &str, default: &str) -> CResult<String> {
        self.get_option(
            &|c, s, o| Ok(c.get(s, o)?),
            option,
            Some(default.to_string()),
            None,
            false,
            &|v| Value::String(v.clone()),
        )
    }

    /// get() with a default of any type, the default is recorded as the
    /// parsed value like python's get(option, default)
    pub fn get_value_or(&self, option: &str, default: Value, deprecate: bool) -> CResult<Value> {
        self.get_option(&|c, s, o| Ok(Value::String(c.get(s, o)?)), option, Some(default), None, deprecate, &|v| v.clone())
    }

    pub fn get_opt(&self, option: &str) -> CResult<Option<String>> {
        self.get_opt_dep(option, false)
    }

    pub fn get_opt_dep(&self, option: &str, deprecate: bool) -> CResult<Option<String>> {
        self.get_option(
            &|c, s, o| Ok(Some(c.get(s, o)?)),
            option,
            Some(None),
            None,
            deprecate,
            &|v| v.clone().map(Value::String).unwrap_or(Value::Null),
        )
    }

    // ***** Integer options *****

    fn parse_int(c: &ConfigParser, s: &str, o: &str) -> Result<i64, FetchErr> {
        let raw = c.get(s, o)?;
        crate::pyfmt::py_int(&raw)
            .ok_or_else(|| FetchErr::Other(format!("invalid literal for int() with base 10: '{}'", raw)))
    }

    pub fn getint_b(&self, option: &str, default: Option<i64>, bounds: Bounds) -> CResult<i64> {
        let ex = |v: &i64| Some((*v as f64, true));
        self.get_option(&Self::parse_int, option, default, Some((&ex, bounds)), false, &|v| Value::from(*v))
    }

    pub fn getint(&self, option: &str) -> CResult<i64> {
        self.getint_b(option, None, Bounds::none())
    }

    pub fn getint_or(&self, option: &str, default: i64) -> CResult<i64> {
        self.getint_b(option, Some(default), Bounds::none())
    }

    pub fn getint_opt(&self, option: &str, bounds: Bounds) -> CResult<Option<i64>> {
        let ex = |v: &Option<i64>| v.map(|x| (x as f64, true));
        self.get_option(
            &|c, s, o| Self::parse_int(c, s, o).map(Some),
            option,
            Some(None),
            Some((&ex, bounds)),
            false,
            &|v| v.map(Value::from).unwrap_or(Value::Null),
        )
    }

    // ***** Float options *****

    fn parse_float(c: &ConfigParser, s: &str, o: &str) -> Result<f64, FetchErr> {
        let raw = c.get(s, o)?;
        crate::pyfmt::py_float(&raw).ok_or_else(|| FetchErr::Other(format!("could not convert string to float: '{}'", raw)))
    }

    pub fn getfloat_b(&self, option: &str, default: Option<f64>, bounds: Bounds) -> CResult<f64> {
        let ex = |v: &f64| Some((*v, false));
        self.get_option(&Self::parse_float, option, default, Some((&ex, bounds)), false, &|v| utils::float_value(*v))
    }

    pub fn getfloat_or(&self, option: &str, default: f64) -> CResult<f64> {
        self.getfloat_b(option, Some(default), Bounds::none())
    }

    pub fn getfloat_opt(&self, option: &str, bounds: Bounds) -> CResult<Option<f64>> {
        let ex = |v: &Option<f64>| v.map(|x| (x, false));
        self.get_option(
            &|c, s, o| Self::parse_float(c, s, o).map(Some),
            option,
            Some(None),
            Some((&ex, bounds)),
            false,
            &|v| v.map(utils::float_value).unwrap_or(Value::Null),
        )
    }

    // ***** Boolean options *****

    fn parse_bool(c: &ConfigParser, s: &str, o: &str) -> Result<bool, FetchErr> {
        let raw = c.get(s, o)?;
        match raw.to_lowercase().as_str() {
            "1" | "yes" | "true" | "on" => Ok(true),
            "0" | "no" | "false" | "off" => Ok(false),
            _ => Err(FetchErr::Other(format!("Not a boolean: {}", raw))),
        }
    }

    pub fn getboolean_or(&self, option: &str, default: bool) -> CResult<bool> {
        self.get_option(&Self::parse_bool, option, Some(default), None, false, &|v| Value::Bool(*v))
    }

    pub fn getboolean_dep(&self, option: &str, default: bool, deprecate: bool) -> CResult<bool> {
        self.get_option(&Self::parse_bool, option, Some(default), None, deprecate, &|v| Value::Bool(*v))
    }

    pub fn getboolean_opt(&self, option: &str) -> CResult<Option<bool>> {
        self.get_option(
            &|c, s, o| Self::parse_bool(c, s, o).map(Some),
            option,
            Some(None),
            None,
            false,
            &|v| v.map(Value::Bool).unwrap_or(Value::Null),
        )
    }

    // ***** Choice *****

    pub fn getchoice(&self, option: &str, choices: &[&str], default_key: Option<&str>, force_lowercase: bool) -> CResult<String> {
        let mut result = match default_key {
            None => self.get(option)?,
            Some(d) => self.get_or(option, d)?,
        };
        if force_lowercase {
            result = result.to_lowercase();
        }
        if !choices.contains(&result.as_str()) {
            let items: Vec<Value> = choices.iter().map(|c| Value::String(c.to_string())).collect();
            return Err(ConfigError(format!(
                "Section [{}], Option '{}: Value {} is not a vailid choice.  Must be one of the following {}",
                self.section,
                option,
                result,
                crate::pyfmt::py_repr(&Value::Array(items))
            )));
        }
        Ok(result)
    }

    // ***** Lists *****

    fn split_sep<'a>(value: &'a str, sep: Option<&str>) -> Vec<&'a str> {
        match sep {
            None => value.split_whitespace().collect(),
            Some(s) => value.split(s).collect(),
        }
    }

    fn list_parser(
        value: &str,
        conv: &dyn Fn(&str) -> Result<Value, String>,
        seps: &[Option<&str>],
        counts: &[Option<usize>],
    ) -> Result<Value, String> {
        let sep = seps[0];
        let cnt = counts[0];
        let mut ret: Vec<Value> = Vec::new();
        if seps.len() > 1 {
            for sub in Self::split_sep(value, sep).into_iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                ret.push(Self::list_parser(sub, conv, &seps[1..], &counts[1..])?);
            }
        } else {
            for v in Self::split_sep(value, sep).into_iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                ret.push(conv(v)?);
            }
        }
        if let Some(c) = cnt {
            if ret.len() != c {
                return Err(format!("List length mismatch, expected {}, parsed {}", c, ret.len()));
            }
        }
        Ok(Value::Array(ret))
    }

    /// General list parser, returns nested json arrays
    pub fn getlists(
        &self,
        option: &str,
        default: Option<Value>,
        conv: &dyn Fn(&str) -> Result<Value, String>,
        separators: &[Option<&str>],
        count: Option<&[Option<usize>]>,
    ) -> CResult<Value> {
        let counts: Vec<Option<usize>> = match count {
            Some(c) => {
                if c.len() != separators.len() {
                    return Err(ConfigError(format!(
                        "Option '{}' in section [{}]: length of 'count' argument must match length of 'separators' argument",
                        option, self.section
                    )));
                }
                c.to_vec()
            }
            None => vec![None; separators.len()],
        };
        self.get_option(
            &|c, s, o| {
                let raw = c.get(s, o)?;
                Self::list_parser(&raw, conv, separators, &counts).map_err(FetchErr::Other)
            },
            option,
            default,
            None,
            false,
            &|v| v.clone(),
        )
    }

    fn str_conv(v: &str) -> Result<Value, String> {
        Ok(Value::String(v.to_string()))
    }

    fn to_str_vec(v: Value) -> Vec<String> {
        v.as_array().map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default()
    }

    pub fn getlist_sep(&self, option: &str, sep: Option<&str>, count: Option<usize>) -> CResult<Vec<String>> {
        Ok(Self::to_str_vec(self.getlists(option, None, &Self::str_conv, &[sep], Some(&[count]))?))
    }

    /// getlist with a default.  Returns None when the default is None.
    pub fn getlist_opt(&self, option: &str, default: Option<Vec<String>>, sep: Option<&str>) -> CResult<Option<Vec<String>>> {
        let d = match &default {
            Some(v) => Value::Array(v.iter().map(|s| Value::String(s.clone())).collect()),
            None => Value::Null,
        };
        let v = self.getlists(option, Some(d), &Self::str_conv, &[sep], None)?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(Self::to_str_vec(v)))
    }

    pub fn getintlist_opt(&self, option: &str, default: Option<Vec<i64>>, sep: Option<&str>, count: Option<usize>) -> CResult<Option<Vec<i64>>> {
        let d = match &default {
            Some(v) => Value::Array(v.iter().map(|x| Value::from(*x)).collect()),
            None => Value::Null,
        };
        let conv = |s: &str| {
            crate::pyfmt::py_int(s).map(Value::from).ok_or_else(|| format!("invalid literal for int() with base 10: '{}'", s))
        };
        let v = self.getlists(option, Some(d), &conv, &[sep], Some(&[count]))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(v.as_array().unwrap().iter().filter_map(|x| x.as_i64()).collect()))
    }

    // ***** Dict *****

    pub fn getdict(
        &self,
        option: &str,
        default: Option<Value>,
        separators: (Option<&str>, &str),
        allow_empty_fields: bool,
    ) -> CResult<Value> {
        self.getdict_conv(option, default, separators, allow_empty_fields, &Self::str_conv)
    }

    pub fn getdict_conv(
        &self,
        option: &str,
        default: Option<Value>,
        separators: (Option<&str>, &str),
        allow_empty_fields: bool,
        conv: &dyn Fn(&str) -> Result<Value, String>,
    ) -> CResult<Value> {
        self.get_option(
            &|c, s, o| {
                let raw = c.get(s, o)?;
                let mut ret = Map::new();
                for line in Self::split_sep(&raw, separators.0) {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match line.split_once(separators.1) {
                        None => {
                            if allow_empty_fields {
                                ret.insert(line.to_string(), Value::Null);
                            } else {
                                return Err(FetchErr::Other(format!("Failed to parse dictionary field, {}", line)));
                            }
                        }
                        Some((k, v)) => {
                            ret.insert(k.trim().to_string(), conv(v.trim()).map_err(FetchErr::Other)?);
                        }
                    }
                }
                Ok(Value::Object(ret))
            },
            option,
            default,
            None,
            false,
            &|v| v.clone(),
        )
    }

    // ***** Templates and paths *****

    pub fn gettemplate(&self, option: &str, is_async: bool) -> CResult<crate::components::template::JinjaTemplate> {
        self.gettemplate_opt(option, None, is_async)?
            .ok_or_else(|| ConfigError(format!("No option '{}' in section: '{}'", option, self.section)))
    }

    /// Returns Ok(None) only when a None default is supplied (Some(None)) and the option is missing
    pub fn gettemplate_opt(
        &self,
        option: &str,
        default: Option<Option<String>>,
        is_async: bool,
    ) -> CResult<Option<crate::components::template::JinjaTemplate>> {
        let factory = crate::components::template::get_factory(self).map_err(|_| {
            ConfigError(format!("Section [{}], option '{}': Failed to load 'template' component.", self.section, option))
        })?;
        // Default values that are strings are returned as-is (not templates)
        // by python's gettemplate; callers that need a template call
        // load_template which converts them.
        let res: Option<Result<crate::components::template::JinjaTemplate, ConfigError>> = self.get_option(
            &|c, s, o| {
                let raw = c.get(s, o)?;
                Ok(Some(factory.create_template(raw.trim(), is_async).map_err(|e| ConfigError(e.message))))
            },
            option,
            default.map(|d| d.map(|s| factory.create_template(s.trim(), is_async).map_err(|e| ConfigError(e.message)))),
            None,
            false,
            &|v| match v {
                Some(Ok(t)) => Value::String(t.source().to_string()),
                _ => Value::Null,
            },
        )?;
        match res {
            None => Ok(None),
            Some(r) => r.map(Some).map_err(|e| {
                self.mark_error();
                ConfigError(format!(
                    "[{}]: Option '{}' encountered the following error while parsing: {}",
                    self.section, option, e
                ))
            }),
        }
    }

    pub fn load_template(&self, option: &str, default: Option<&str>, is_async: bool) -> CResult<crate::components::template::JinjaTemplate> {
        match default {
            None => self.gettemplate(option, is_async),
            Some(d) => Ok(self.gettemplate_opt(option, Some(Some(d.to_string())), is_async)?.unwrap()),
        }
    }

    fn render_path(&self, tmpl: &crate::components::template::JinjaTemplate) -> CResult<PathBuf> {
        let data_path = self.server().map(|s| s.data_path()).unwrap_or_default();
        let mut ctx = Map::new();
        ctx.insert("data_path".into(), Value::String(data_path));
        let strpath = tmpl.render(&Value::Object(ctx)).map_err(|e| ConfigError(e.message))?;
        Ok(utils::resolve_path(&utils::expand_user(&strpath)))
    }

    pub fn getpath_or(&self, option: &str, default: &Path) -> CResult<PathBuf> {
        self.getpath_opt_dep(option, Some(default), false).map(|p| p.unwrap())
    }

    pub fn getpath_opt_dep(&self, option: &str, default: Option<&Path>, deprecate: bool) -> CResult<Option<PathBuf>> {
        let factory = crate::components::template::get_factory(self).map_err(|_| {
            ConfigError(format!("Section [{}], option '{}': Failed to load 'template' component.", self.section, option))
        })?;
        // In python the default (a Path) is returned as is when the option is
        // missing.  Path objects are recorded in the parsed config via str()
        let res: Option<Result<crate::components::template::JinjaTemplate, ConfigError>> = self.get_option(
            &|c, s, o| {
                let raw = c.get(s, o)?;
                Ok(Some(factory.create_template(raw.trim(), false).map_err(|e| ConfigError(e.message))))
            },
            option,
            Some(None),
            None,
            deprecate,
            &|v| match v {
                Some(Ok(t)) => Value::String(t.source().to_string()),
                _ => match default {
                    Some(d) => Value::String(d.to_string_lossy().to_string()),
                    None => Value::Null,
                },
            },
        )?;
        match res {
            None => Ok(default.map(|d| d.to_path_buf())),
            Some(Err(e)) => {
                self.mark_error();
                Err(ConfigError(format!(
                    "[{}]: Option '{}' encountered the following error while parsing: {}",
                    self.section, option, e
                )))
            }
            Some(Ok(t)) => self.render_path(&t).map(Some),
        }
    }

    // ***** Supplemental configs *****

    pub fn read_supplemental_dict(&self, obj: &Value) -> CResult<ConfigHelper> {
        let map = obj.as_object().filter(|m| !m.is_empty()).ok_or_else(|| ConfigError("Cannot ready Empty Dict".into()))?;
        let mut cp = ConfigParser::new();
        for (sec, opts) in map {
            if sec != crate::configparser::DEFAULT_SECTION {
                cp.add_section(sec).map_err(|e| ConfigError(format!("Error Reading config as dict: {}", e)))?;
            }
            if let Some(o) = opts.as_object() {
                for (k, v) in o {
                    let sval = match v {
                        Value::String(s) => s.clone(),
                        other => crate::pyfmt::py_str(other),
                    };
                    cp.set(sec, k, &sval).map_err(|e| ConfigError(e.to_string()))?;
                }
            }
        }
        let first = cp.sections().first().cloned().unwrap_or_default();
        let source = Rc::new(ConfigSource::new_dict(cp));
        Ok(ConfigHelper::new(source, &first, Rc::new(RefCell::new(IndexMap::new())), None))
    }

    /// read_supplemental_config(file_name): the file's first section (None:
    /// the file has none, sections[0] raises there)
    pub fn read_supplemental_config(&self, file_name: &str) -> CResult<Option<ConfigHelper>> {
        let fpath = utils::resolve_path(&utils::expand_user(file_name));
        let source = Rc::new(ConfigSource::new_file());
        source.read_file(&fpath)?;
        let first = match source.config.borrow().sections().first() {
            Some(s) => s.clone(),
            None => return Ok(None),
        };
        Ok(Some(ConfigHelper::new(source, &first, Rc::new(RefCell::new(IndexMap::new())), None)))
    }

    pub fn write_config(&self) -> String {
        self.source.config.borrow().write()
    }

    pub fn get_parsed_config(&self) -> Value {
        let parsed = self.parsed.borrow();
        let mut out = Map::new();
        for (k, v) in parsed.iter() {
            out.insert(k.clone(), Value::Object(v.clone()));
        }
        Value::Object(out)
    }

    pub fn get_orig_config(&self) -> Value {
        let d = self.source.config.borrow().as_dict();
        let mut out = Map::new();
        for (k, v) in d {
            let m: Map<String, Value> = v.into_iter().map(|(a, b)| (a, Value::String(b))).collect();
            out.insert(k, Value::Object(m));
        }
        Value::Object(out)
    }

    pub fn get_file_sections(&self) -> IndexMap<String, Vec<String>> {
        self.source.get_file_sections()
    }

    pub fn get_config_files(&self) -> Vec<String> {
        self.source.files.borrow().iter().map(|f| f.to_string_lossy().to_string()).collect()
    }

    pub fn validate_config(&self, server: &Server) {
        let cfg = self.source.config.borrow();
        let parsed = self.parsed.borrow();
        for sect in cfg.sections() {
            let parsed_opts = match parsed.get(&sect) {
                None => {
                    server.add_warning(
                        &format!(
                            "Unparsed config section [{}] detected.  This may be the result of a component that failed to load.  In the future this will result in a startup error.",
                            sect
                        ),
                        None,
                        true,
                    );
                    continue;
                }
                Some(p) => p,
            };
            if parsed_opts.contains_key(CFG_ERROR_KEY) {
                continue;
            }
            for (opt, val) in cfg.items(&sect).unwrap_or_default() {
                if !parsed_opts.contains_key(&opt) {
                    server.add_warning(
                        &format!(
                            "Unparsed config option '{}: {}' detected in section [{}].  This may be an option no longer available or could be the result of a module that failed to load.  In the future this will result in a startup error.",
                            opt, val, sect
                        ),
                        None,
                        true,
                    );
                }
            }
        }
    }

    pub fn create_backup(&self, config_file: &str) {
        let cfg = utils::expand_user(config_file);
        let name = cfg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let backup = cfg.parent().unwrap_or(Path::new("/")).join(format!(".{}.bkp", name));
        if backup.exists() {
            let mut cfg_mtime: i128 = 0;
            for f in self.source.files.borrow().iter() {
                cfg_mtime = cfg_mtime.max(utils::mtime_ns(f).unwrap_or(0));
            }
            let bkp_mtime = utils::mtime_ns(&backup).unwrap_or(0);
            if bkp_mtime >= cfg_mtime {
                return;
            }
        }
        match std::fs::write(&backup, self.source.config.borrow().write()) {
            Ok(_) => log::info!("Backing up last working configuration to '{}'", backup.display()),
            Err(e) => log::error!("Failed to create a backup: {}", e),
        }
    }
}
