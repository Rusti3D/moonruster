// GCode metadata extraction utility (port of moonraker/components/file_manager/metadata.py)
//
// Run as a subprocess of the main server: `moonruster --metadata -c <config.json>`
mod cancellation;


use base64::Engine;
use fancy_regex::Regex as FRegex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const READ_SIZE: usize = 1024 * 1024;
const UFP_MODEL_PATH: &str = "/3D/model.gcode";
const UFP_THUMB_PATH: &str = "/Metadata/thumbnail.png";
const SUPPORTED_THUMB_FORMATS: [&str; 3] = ["png", "jpg", "qoi"];
const PPC_REGEX: &str = r"^; Pre-Processed for Cancel-Object support by preprocess_cancellation (?P<version>v?\d+(?:\.\d+)*)";

fn logi(msg: &str) {
    eprintln!("INFO:metadata:{}", msg);
}

fn re(p: &str) -> Regex {
    crate::pycompat::re::try_compile(p).unwrap_or_else(|| crate::pycompat::re::compile("$^"))
}

fn sub_float(p: &str) -> String {
    p.replace("(%F)", r"([0-9]*\.?[0-9]+)")
}
fn sub_int(p: &str) -> String {
    p.replace("(%D)", r"([0-9]+)")
}
fn sub_str(p: &str) -> String {
    p.replace("(%S)", r"(.*)")
}

fn py_float(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

pub fn regex_find_floats(pattern: &str, data: &str) -> Vec<f64> {
    let r = re(&sub_float(pattern));
    let mut out = Vec::new();
    for c in r.captures_iter(data) {
        match c.get(1).and_then(|m| py_float(m.as_str())) {
            Some(f) => out.push(f),
            None => return Vec::new(),
        }
    }
    out
}

pub fn regex_find_ints(pattern: &str, data: &str) -> Vec<i64> {
    let r = re(&sub_int(pattern));
    let mut out = Vec::new();
    for c in r.captures_iter(data) {
        match c.get(1).and_then(|m| m.as_str().parse::<i64>().ok()) {
            Some(f) => out.push(f),
            None => return Vec::new(),
        }
    }
    out
}

pub fn regex_find_strings(pattern: &str, separators: &str, data: &str) -> Vec<String> {
    let r = re(&sub_str(pattern));
    let m = match r.captures(data).and_then(|c| c.get(1)) {
        Some(m) if !m.as_str().is_empty() => m.as_str().to_string(),
        _ => return Vec::new(),
    };
    let seps = regex::escape(separators);
    let item_re = re(&format!(r#"\s*(")(?:\\"|[^"])*"\s*|[^{}]+"#, seps));
    let mut out = Vec::new();
    for c in item_re.captures_iter(&m) {
        let mut val = c.get(0).unwrap().as_str().trim().to_string();
        if let Some(sep) = c.get(1) {
            let sep = sep.as_str();
            let inner: String = val.chars().skip(1).collect::<String>();
            let inner: String = inner.chars().take(inner.chars().count().saturating_sub(1)).collect();
            val = inner.replace(&format!("\\{}", sep), sep).trim().to_string();
        }
        if !val.is_empty() {
            out.push(val);
        }
    }
    out
}

pub fn regex_find_float(pattern: &str, data: &str) -> Option<f64> {
    re(&sub_float(pattern)).captures(data).and_then(|c| c.get(1)).and_then(|m| py_float(m.as_str()))
}

pub fn regex_find_int(pattern: &str, data: &str) -> Option<i64> {
    re(&sub_int(pattern)).captures(data).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse().ok())
}

pub fn regex_find_string(pattern: &str, data: &str) -> Option<String> {
    re(&sub_str(pattern)).captures(data).map(|c| c.get(1).map(|m| m.as_str()).unwrap_or("").trim_matches('"').to_string())
}

fn regex_find_min_float(p: &str, d: &str) -> Option<f64> {
    regex_find_floats(p, d).into_iter().fold(None, |acc, x| Some(acc.map_or(x, |a: f64| a.min(x))))
}

fn regex_find_max_float(p: &str, d: &str) -> Option<f64> {
    regex_find_floats(p, d).into_iter().fold(None, |acc, x| Some(acc.map_or(x, |a: f64| a.max(x))))
}

fn fval(v: Option<f64>) -> Option<Value> {
    v.map(crate::pycompat::pyfmt::float_value)
}

fn ival(v: Option<i64>) -> Option<Value> {
    v.map(|i| json!(i))
}

fn sval(v: Option<String>) -> Option<Value> {
    v.map(Value::String)
}

fn empty_to_none(v: String) -> Option<String> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// bytes.decode(errors="ignore")
fn decode_ignore(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for chunk in b.utf8_chunks() {
        s.push_str(chunk.valid());
    }
    s
}

/// first n chars of a string
fn char_head(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// last n chars of a string
fn char_tail(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    match s.char_indices().nth(skip) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Unknown,
    PrusaSlicer,
    Slic3rPE,
    Slic3r,
    BambuStudio,
    Cura,
    Simplify3D,
    KISSlicer,
    IdeaMaker,
    IceSL,
    KiriMoto,
}

impl Kind {
    fn is_prusa(&self) -> bool {
        matches!(self, Kind::PrusaSlicer | Kind::Slic3rPE | Kind::Slic3r | Kind::BambuStudio)
    }
}

/// Identification order mirrors the python registration order (children before parents)
const IDENT_ORDER: [Kind; 10] = [
    Kind::Slic3rPE,
    Kind::Slic3r,
    Kind::BambuStudio,
    Kind::PrusaSlicer,
    Kind::Cura,
    Kind::Simplify3D,
    Kind::KISSlicer,
    Kind::IdeaMaker,
    Kind::IceSL,
    Kind::KiriMoto,
];

fn identify(kind: Kind, data: &str) -> Option<(String, String)> {
    let cap = |p: &str| re(p).captures(data).map(|c| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default());
    match kind {
        Kind::Unknown => None,
        Kind::PrusaSlicer => {
            let m = re(r"; generated by (.+?) on \d{4}-\d{2}-\d{2} at \d{2}:\d{2}:\d{2}").captures(data);
            match m {
                None => {
                    let aliases = ["PrusaSlicer", "SuperSlicer", "OrcaSlicer", "MomentSlicer", "SliCR-3D", "A3dp-Slicer", "QIDISlicer"];
                    let pat = format!(r"({})\s(.*)\son", aliases.join("|"));
                    let c = re(&pat).captures(data)?;
                    let name = c.get(1).unwrap().as_str().to_string();
                    logi(&format!("Detected PrusaSlicer fork via fallback: {}", name));
                    Some((name, c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default()))
                }
                Some(c) => {
                    let full = c.get(1).unwrap().as_str();
                    let (name, version) = match full.rfind(' ') {
                        Some(i) => (&full[..i], &full[i + 1..]),
                        None => ("", full),
                    };
                    let exclusions = ["slic3r prusa edition", "slic3r", "bambustudio"];
                    if exclusions.contains(&name.to_lowercase().as_str()) {
                        return None;
                    }
                    Some((name.to_string(), version.to_string()))
                }
            }
        }
        Kind::Slic3rPE => cap(r"Slic3r\sPrusa\sEdition\s(.*)\son").map(|v| ("Slic3r PE".into(), v)),
        Kind::Slic3r => cap(r"Slic3r\s(\d.*)\son").map(|v| ("Slic3r".into(), v)),
        Kind::BambuStudio => cap(r"; BambuStudio[^ ]* ([^\n]+)").map(|v| ("BambuStudio".into(), v)),
        Kind::Cura => cap(r"Cura_SteamEngine\s(.*)").map(|v| ("Cura".into(), v)),
        Kind::Simplify3D => cap(r"Simplify3D\(R\)\sVersion\s(.*)").map(|v| ("Simplify3D".into(), v)),
        Kind::KISSlicer => {
            if re(r";\sKISSlicer").is_match(data) {
                let version = cap(r";\sversion\s(.*)").map(|v| v.replace(' ', "-")).unwrap_or_else(|| "?".into());
                Some(("KISSlicer".into(), version))
            } else {
                None
            }
        }
        Kind::IdeaMaker => cap(r"\sideaMaker\s(.*),").map(|v| ("IdeaMaker".into(), v)),
        Kind::IceSL => cap(r"<IceSL\s(.*)>").map(|v| {
            let ver = if v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) { v } else { "-".into() };
            ("IceSL".into(), ver)
        }),
        Kind::KiriMoto => {
            if let Some(v) = cap(r"; Generated by Kiri:Moto (\d.+)") {
                return Some(("Kiri:Moto".into(), v));
            }
            cap(r"; Generated by Kiri:Moto \(SimplyPrint\) (.+)").map(|v| ("SimplyPrint".into(), v))
        }
    }
}

pub struct Slicer {
    kind: Kind,
    pub slicer_name: String,
    pub slicer_version: String,
    path: PathBuf,
    layer_height: Option<f64>,
    pub has_m486_objects: bool,
    file_data: String,
    header_data: String,
    footer_data: String,
    size: u64,
}

impl Slicer {
    pub fn from_file(path: &Path) -> std::io::Result<Slicer> {
        let mut f = std::fs::File::open(path)?;
        let size = f.seek(SeekFrom::End(0))?;
        f.seek(SeekFrom::Start(0))?;
        let mut hbuf = vec![0u8; READ_SIZE.min(size as usize)];
        f.read_exact(&mut hbuf)?;
        let header = decode_ignore(&hbuf);
        let mut tail = String::new();
        if size > (READ_SIZE * 2) as u64 {
            f.seek(SeekFrom::Start(size - READ_SIZE as u64))?;
        }
        if size > READ_SIZE as u64 {
            let mut tbuf = Vec::new();
            f.read_to_end(&mut tbuf)?;
            tail = decode_ignore(&tbuf);
        }
        let data = format!("{}{}", header, tail);
        let (kind, name, ver) = IDENT_ORDER
            .iter()
            .find_map(|k| identify(*k, &header).map(|(n, v)| (*k, n, v)))
            .unwrap_or((Kind::Unknown, "Unknown".into(), "?".into()));
        Ok(Slicer {
            kind,
            slicer_name: name,
            slicer_version: ver,
            path: path.to_path_buf(),
            layer_height: None,
            has_m486_objects: false,
            header_data: char_head(&data, READ_SIZE).to_string(),
            footer_data: char_tail(&data, READ_SIZE).to_string(),
            file_data: data,
            size,
        })
    }

    fn config_data(&self) -> &str {
        if self.kind == Kind::BambuStudio {
            &self.header_data
        } else {
            &self.footer_data
        }
    }

    fn check_has_objects(&mut self, data: &str, pattern: Option<&str>) -> bool {
        if let Some(c) = re(r"\n((DEFINE_OBJECT)|(EXCLUDE_OBJECT_DEFINE)) NAME=").captures(data) {
            let fname = self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            logi(&format!("File '{}' currently supports cancellation, processing aborted", fname));
            if c.get(1).unwrap().as_str().starts_with("DEFINE_OBJECT") {
                logi("Legacy object processing detected.  This is not compatible with official versions of Klipper.");
            }
            return false;
        }
        let mut patterns = vec![r"\nM486"];
        if let Some(p) = pattern {
            patterns.push(p);
        }
        for p in patterns {
            if re(p).is_match(data) {
                self.has_m486_objects = p == r"\nM486";
                return true;
            }
        }
        false
    }

    pub fn has_objects(&mut self) -> bool {
        let header = self.header_data.clone();
        let pat = match self.kind {
            k if k.is_prusa() => Some(r"\n; printing object"),
            Kind::Cura => Some(r"\n;MESH:"),
            Kind::IdeaMaker => Some(r"\n;PRINTING:"),
            _ => None,
        };
        self.check_has_objects(&header, pat)
    }

    pub fn check_gcode_processor(&self, regex: &str, location: &str) -> Option<Map<String, Value>> {
        let data = if location == "header" { &self.header_data } else { &self.footer_data };
        let r = crate::pycompat::re::try_compile(&format!("(?m){}", regex))?;
        let c = r.captures(data)?;
        let mut m = Map::new();
        for name in r.capture_names().flatten() {
            m.insert(name.to_string(), c.name(name).map(|v| json!(v.as_str())).unwrap_or(Value::Null));
        }
        Some(m)
    }

    fn parse_gcode_start_byte(&self) -> Option<Value> {
        let m = re(r"\n[MG]\d+\s.*\n").find(&self.header_data)?;
        Some(json!(self.header_data[..m.start()].len()))
    }

    fn parse_gcode_end_byte(&self) -> Option<Value> {
        let rev: String = self.footer_data.chars().rev().collect();
        let m = re(r"\n.*\s\d+[MG]\n").find(&rev)?;
        Some(json!(self.size as i64 - rev[..m.start()].len() as i64))
    }

    fn parse_layer_count(&self) -> Option<Value> {
        let cd = self.config_data();
        match self.kind {
            k if k == Kind::BambuStudio => ival(regex_find_int(r"; total layer number: (%D)", cd)),
            k if k.is_prusa() => ival(regex_find_int(r"; total layers count = (%D)", cd)),
            Kind::Cura => ival(regex_find_int(r";LAYER_COUNT\:(%D)", &self.header_data)),
            Kind::IceSL => ival(regex_find_int(r";\slayer_count\s:\s+(%D)", &self.header_data)),
            Kind::KiriMoto => {
                let ms: Vec<i64> = re(r";; --- layer (\d+) \(.+")
                    .captures_iter(&self.footer_data)
                    .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse().ok()))
                    .collect();
                ms.last().map(|l| json!(l + 1))
            }
            _ => None,
        }
    }

    fn parse_object_height(&self) -> Option<Value> {
        let h = &self.header_data;
        let f = &self.footer_data;
        match self.kind {
            Kind::Unknown | Kind::Simplify3D => fval(regex_find_max_float(r"G1\sZ(%F)\s", f)),
            Kind::BambuStudio => fval(regex_find_float(r"; max_z_height: (%F)", self.config_data())),
            k if k.is_prusa() => {
                let ms: Vec<String> = re(r";BEFORE_LAYER_CHANGE\n(?:.*\n)?;(\d+\.?\d*)")
                    .captures_iter(f)
                    .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
                    .collect();
                if !ms.is_empty() {
                    let parsed: Option<Vec<f64>> = ms.iter().map(|m| py_float(m)).collect();
                    if let Some(p) = parsed {
                        return fval(p.into_iter().fold(None, |a: Option<f64>, x| Some(a.map_or(x, |y| y.max(x)))));
                    }
                }
                fval(regex_find_max_float(r"G1\sZ(%F)\sF", f))
            }
            Kind::Cura => fval(regex_find_float(r";MAXZ:(%F)", h)),
            Kind::KISSlicer => fval(regex_find_max_float(r";\sEND_LAYER_OBJECT\sz=(%F)", f)),
            Kind::IdeaMaker => fval(regex_find_float(r";Bounding Box:(?:\s+(%F))+", h)),
            Kind::IceSL => fval(regex_find_float(r";\sprint_height_mm\s:\s+(%F)", h)),
            Kind::KiriMoto => fval(regex_find_max_float(r"G1 Z(%F) (?:; z-hop end|F\d+\n)", f)),
            _ => None,
        }
    }

    fn parse_estimated_time(&self) -> Option<Value> {
        let h = &self.header_data;
        let f = &self.footer_data;
        match self.kind {
            Kind::Slic3r => None,
            k if k.is_prusa() => {
                let tm = re(r";\sestimated\sprinting\stime.*").find(self.config_data())?;
                let tg = tm.as_str();
                let mut total: i64 = 0;
                for (p, mult) in [(r"(\d+)d", 86400), (r"(\d+)h", 3600), (r"(\d+)m", 60), (r"(\d+)s", 1)] {
                    if let Some(c) = re(p).captures(tg) {
                        total += c[1].parse::<i64>().ok()? * mult;
                    }
                }
                Some(json!(total))
            }
            Kind::Cura => fval(regex_find_max_float(r";TIME:(%F)", h)),
            Kind::Simplify3D => {
                let tm = re(r";\s+Build (t|T)ime:.*").find(f)?;
                let tg = tm.as_str();
                let mut total: i64 = 0;
                for (p, mult) in [(r"(\d+)\shours?", 3600), (r"(\d+)\smin", 60), (r"(\d+)\ssec", 1)] {
                    if let Some(c) = re(p).captures(tg) {
                        total += c[1].parse::<i64>().ok()? * mult;
                    }
                }
                Some(json!(total))
            }
            Kind::KISSlicer => regex_find_float(r";\sCalculated.*Build\sTime:\s(%F)\sminutes", f)
                .map(|t| crate::pycompat::pyfmt::float_value(crate::pycompat::pyfmt::py_round(t * 60.0, 2))),
            Kind::IdeaMaker => fval(regex_find_float(r";Print\sTime:\s(%F)", f)),
            Kind::IceSL => fval(regex_find_float(r";\sestimated_print_time_s\s:\s+(%F)", h)),
            Kind::KiriMoto => ival(regex_find_int(r"; --- print time: (%D)s", f)),
            _ => None,
        }
    }

    fn parse_nozzle_diameter(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            k if k.is_prusa() => fval(regex_find_float(r";\snozzle_diameter\s=\s(%F)", self.config_data())),
            Kind::Cura => fval(regex_find_float(r";Nozzle\sdiameter\s=\s(%F)", h)),
            Kind::Simplify3D => fval(regex_find_float(r";\s+(?:extruderDiameter|nozzleDiameter),(%F)", h)),
            Kind::IdeaMaker => fval(regex_find_float(r";Dimension:(?:\s\d+\.\d+){3}\s(%F)", h)),
            Kind::IceSL => fval(regex_find_float(r";\snozzle_diameter_mm_0\s:\s+(%F)", h)),
            _ => None,
        }
    }

    fn parse_layer_height(&mut self) -> Option<Value> {
        let h = self.header_data.clone();
        let v = match self.kind {
            Kind::BambuStudio | Kind::PrusaSlicer | Kind::Slic3rPE | Kind::Slic3r => {
                let v = regex_find_float(r"; layer_height = (%F)", self.config_data());
                self.layer_height = v;
                v
            }
            Kind::Cura => {
                let v = regex_find_float(r";Layer\sheight:\s(%F)", &h);
                self.layer_height = v;
                v
            }
            Kind::Simplify3D => {
                let v = regex_find_float(r";\s+layerHeight,(%F)", &h);
                self.layer_height = v;
                v
            }
            Kind::KISSlicer => {
                let v = regex_find_float(r";\s+max_layer_thickness_mm\s=\s(%F)", &h);
                self.layer_height = v;
                v
            }
            Kind::IdeaMaker => regex_find_float(r";LAYER:1\s*.*\s*;HEIGHT:(%F)", &h),
            Kind::IceSL => {
                let v = regex_find_float(r";\sz_layer_height_mm\s:\s+(%F)", &h);
                self.layer_height = v;
                v
            }
            Kind::KiriMoto => {
                let v = regex_find_float(r"; sliceHeight = (%F)", &h);
                self.layer_height = v;
                v
            }
            Kind::Unknown => None,
        };
        fval(v)
    }

    fn parse_first_layer_height(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            Kind::Unknown | Kind::Simplify3D => fval(regex_find_min_float(r"G1\sZ(%F)\s", h)),
            Kind::BambuStudio => fval(regex_find_float(r"; initial_layer_print_height = (%F)", self.config_data())),
            k if k.is_prusa() => {
                let cd = self.config_data();
                if let Some(pct) = regex_find_float(r"; first_layer_height = (%F)%", cd) {
                    let lh = self.layer_height?;
                    return fval(Some(crate::pycompat::pyfmt::py_round(pct / 100.0 * lh, 6)));
                }
                fval(regex_find_float(r"; first_layer_height = (%F)", cd))
            }
            Kind::Cura => fval(regex_find_float(r";MINZ:(%F)", h)),
            Kind::KISSlicer => fval(regex_find_float(r";\s+first_layer_thickness_mm\s=\s(%F)", h)),
            Kind::IdeaMaker => fval(regex_find_float(r";LAYER:0\s*.*\s*;HEIGHT:(%F)", h)),
            Kind::IceSL => fval(regex_find_float(r";\sz_layer_height_first_layer_mm\s:\s+(%F)", h)),
            Kind::KiriMoto => fval(regex_find_float(r"; firstSliceHeight = (%F)", h)),
            _ => None,
        }
    }

    fn s3d_temp_items(&self, pattern: &str) -> Vec<String> {
        match re(pattern).find(&self.header_data) {
            None => Vec::new(),
            Some(m) => m.as_str().split(',').skip(1).map(String::from).collect(),
        }
    }

    fn s3d_first_layer_temp(&self, heater: &str) -> Option<f64> {
        let heaters = self.s3d_temp_items(r"temperatureName.*");
        let temps = self.s3d_temp_items(r"temperatureSetpointTemperatures.*");
        for (h, t) in heaters.iter().zip(temps.iter()) {
            if h == heater {
                return py_float(t);
            }
        }
        None
    }

    fn s3d_first_layer_temp_v5(&self, heater_type: &str) -> Option<f64> {
        let p = format!(
            r"(?ms);\s+temperatureController,.+?;\s+temperatureType,{}.+?;\s+temperatureSetpoints,\d+\|(\d+)",
            heater_type
        );
        re(&p).captures(&self.header_data).and_then(|c| py_float(&c[1]))
    }

    fn parse_first_layer_extr_temp(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            Kind::Unknown | Kind::Cura => fval(regex_find_float(r"M109 S(%F)", h)),
            Kind::BambuStudio => fval(regex_find_float(r"; nozzle_temperature_initial_layer = (%F)", self.config_data())),
            k if k.is_prusa() => fval(regex_find_float(r"; first_layer_temperature = (%F)", self.config_data())),
            Kind::Simplify3D => {
                if self.slicer_version.starts_with('5') {
                    fval(self.s3d_first_layer_temp_v5("extruder"))
                } else {
                    fval(self.s3d_first_layer_temp("Extruder 1"))
                }
            }
            Kind::KISSlicer => fval(regex_find_float(r"; first_layer_C = (%F)", h)),
            Kind::IdeaMaker => fval(regex_find_float(r"M109 T0 S(%F)", h)),
            Kind::IceSL => fval(regex_find_float(r";\sextruder_temp_degree_c_0\s:\s+(%F)", h)),
            Kind::KiriMoto => fval(regex_find_float(r"; firstLayerNozzleTemp = (%F)", h)),
            _ => None,
        }
    }

    fn parse_first_layer_bed_temp(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            Kind::Unknown | Kind::Cura | Kind::IdeaMaker => fval(regex_find_float(r"M190 S(%F)", h)),
            Kind::BambuStudio => fval(regex_find_float(r"; hot_plate_temp_initial_layer = (%F)", self.config_data())),
            k if k.is_prusa() => fval(regex_find_float(r"; first_layer_bed_temperature = (%F)", self.config_data())),
            Kind::Simplify3D => {
                if self.slicer_version.starts_with('5') {
                    fval(self.s3d_first_layer_temp_v5("platform"))
                } else {
                    fval(self.s3d_first_layer_temp("Heated Bed"))
                }
            }
            Kind::KISSlicer => fval(regex_find_float(r"; bed_C = (%F)", h)),
            Kind::IceSL => fval(regex_find_float(r";\sbed_temp_degree_c\s:\s+(%F)", h)),
            Kind::KiriMoto => fval(regex_find_float(r"; firstLayerBedTemp = (%F)", h)),
            _ => None,
        }
    }

    fn parse_chamber_temp(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            Kind::Unknown | Kind::Cura | Kind::IdeaMaker => fval(regex_find_float(r"M191 S(%F)", h)),
            Kind::BambuStudio => fval(regex_find_float(r"; chamber_temperatures = (%F)", self.config_data())),
            k if k.is_prusa() => fval(regex_find_float(r"; chamber_temperature = (%F)", self.config_data())),
            Kind::KISSlicer => fval(regex_find_float(r"; chamber_C = (%F)", h)),
            Kind::IceSL => fval(regex_find_float(r";\schamber_temp_degree_c\s:\s+(%F)", h)),
            _ => None,
        }
    }

    fn multi_or_single(result: Vec<String>) -> Option<Value> {
        if result.len() > 1 {
            // python's json.dumps default separators
            let items: Vec<String> = result.iter().map(|s| py_json_ascii(s)).collect();
            Some(json!(format!("[{}]", items.join(", "))))
        } else {
            result.into_iter().next().map(Value::String)
        }
    }

    fn parse_filament_name(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            k if k.is_prusa() => Self::multi_or_single(regex_find_strings(r";\sfilament_settings_id\s=\s(%S)", ",;", self.config_data())),
            Kind::Cura => sval(regex_find_string(r";Filament\sname\s=\s(%S)", h)),
            Kind::Simplify3D => sval(regex_find_string(r";\s+printMaterial,(%S)", h)),
            Kind::IdeaMaker => sval(
                regex_find_string(r";Filament\sName\s.\d:\s(%S)", h)
                    .and_then(empty_to_none)
                    .or_else(|| regex_find_string(r";Filament\sname\s=\s(%S)", h)),
            ),
            Kind::IceSL => sval(regex_find_string(r";\sfilament_name\s:\s+(%S)", h)),
            _ => None,
        }
    }

    fn parse_filament_type(&self) -> Option<Value> {
        let h = &self.header_data;
        match self.kind {
            k if k.is_prusa() => Self::multi_or_single(regex_find_strings(r";\sfilament_type\s=\s(%S)", ",;", self.config_data())),
            Kind::Cura => sval(regex_find_string(r";Filament\stype\s=\s(%S)", h)),
            Kind::Simplify3D => sval(regex_find_string(r";\s+makerBotModelMaterial,(%S)", &self.footer_data)),
            Kind::IdeaMaker => sval(
                regex_find_string(r";Filament\sType\s.\d:\s(%S)", h)
                    .and_then(empty_to_none)
                    .or_else(|| regex_find_string(r";Filament\stype\s=\s(%S)", h)),
            ),
            Kind::IceSL => sval(regex_find_string(r";\sfilament_type\s:\s+(%S)", h)),
            _ => None,
        }
    }

    fn parse_filament_colors(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            return Some(json!(regex_find_strings(r";\sfilament_colour\s=\s(%S)", ",;", self.config_data())));
        }
        None
    }

    fn parse_filament_change_count(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            let cd = self.config_data();
            if let Some(r) = regex_find_int(r"; total toolchanges = (%D)", cd) {
                return Some(json!(r));
            }
            return ival(regex_find_int(r"; total filament change = (%D)", cd));
        }
        None
    }

    fn parse_extruder_colors(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            return Some(json!(regex_find_strings(r";\sextruder_colour\s=\s(%S)", ",;", self.config_data())));
        }
        None
    }

    fn int_list(vals: Vec<String>) -> Option<Value> {
        let parsed: Option<Vec<i64>> = vals.iter().map(|t| crate::pycompat::pyfmt::py_int(t)).collect();
        parsed.map(|p| json!(p))
    }

    fn parse_filament_temps(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            return Self::int_list(regex_find_strings(r";\s(?:nozzle_)?temperature\s=\s(%S)", ",;", self.config_data()));
        }
        None
    }

    fn parse_referenced_tools(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            return Self::int_list(regex_find_strings(r";\sreferenced_tools\s=\s(%S)", ",;", self.config_data()));
        }
        None
    }

    fn parse_mmu_print(&self) -> Option<Value> {
        if self.kind.is_prusa() {
            return ival(regex_find_int(r";\ssingle_extruder_multi_material\s=\s(%D)", self.config_data()));
        }
        None
    }

    fn parse_filament_total(&self) -> Option<Value> {
        let h = &self.header_data;
        let f = &self.footer_data;
        match self.kind {
            Kind::Slic3rPE => fval(regex_find_float(r"filament\sused\s=\s(%F)mm", f)),
            Kind::Slic3r => fval(regex_find_float(r";\sfilament\_length\_m\s=\s(%F)", f).map(|v| v * 1000.0)),
            Kind::BambuStudio => fval(regex_find_float(r"; total filament length \[mm\] : (%F)", self.config_data())),
            Kind::PrusaSlicer => {
                let line = regex_find_string(r"filament\sused\s\[mm\]\s=\s(%S)\n", self.config_data())?;
                if line.is_empty() {
                    return None;
                }
                let fl = regex_find_floats(r"(%F)", &line);
                if fl.is_empty() {
                    None
                } else {
                    fval(Some(fl.iter().sum()))
                }
            }
            Kind::Cura => {
                let line = regex_find_string(r";Filament\sused:\s(%S)\n", h)?;
                if line.is_empty() {
                    return None;
                }
                let fl = regex_find_floats(r"(%F)", &line);
                if fl.is_empty() {
                    None
                } else {
                    fval(Some(fl.iter().map(|l| l * 1000.0).sum()))
                }
            }
            Kind::Simplify3D => fval(regex_find_float(r";\s+(?:Filament\slength|Material\sLength):\s(%F)\smm", f)),
            Kind::KISSlicer => {
                let fl = regex_find_floats(r";\s+Ext #\d+\s+=\s+(%F)\s*mm", f);
                if fl.is_empty() {
                    None
                } else {
                    fval(Some(fl.iter().sum()))
                }
            }
            Kind::IdeaMaker => {
                let fl = regex_find_floats(r";Material.\d\sUsed:\s+(%F)", f);
                if fl.is_empty() {
                    None
                } else {
                    fval(Some(fl.iter().sum()))
                }
            }
            Kind::IceSL => fval(regex_find_float(r";\sfilament_used_mm\s:\s+(%F)", h)),
            Kind::KiriMoto => fval(regex_find_float(r"; --- filament used: (%F) mm", f)),
            Kind::Unknown => None,
        }
    }

    fn parse_filament_weights(&self) -> Option<Value> {
        match self.kind {
            Kind::BambuStudio => None,
            k if k.is_prusa() => {
                let line = regex_find_string(r"filament\sused\s\[g\]\s=\s(%S)\n", self.config_data())?;
                let w = regex_find_floats(r"(%F)", &line);
                if line.is_empty() || w.is_empty() {
                    None
                } else {
                    Some(json!(w))
                }
            }
            Kind::Cura => {
                let line = regex_find_string(r";Filament\sweight\s=\s\[(%S)\]", &self.header_data)?;
                let w = regex_find_floats(r"(%F)", &line);
                if line.is_empty() || w.is_empty() {
                    None
                } else {
                    Some(json!(w))
                }
            }
            _ => None,
        }
    }

    fn parse_filament_weight_total(&self) -> Option<Value> {
        let h = &self.header_data;
        let f = &self.footer_data;
        match self.kind {
            Kind::Slic3r => fval(regex_find_float(r";\sfilament\smass\_g\s=\s(%F)", f)),
            Kind::BambuStudio => fval(regex_find_float(r"; total filament weight \[g\] : (%F)", self.config_data())),
            k if k.is_prusa() => fval(regex_find_float(r"total\sfilament\sused\s\[g\]\s=\s(%F)", self.config_data())),
            Kind::Cura => {
                let w = self.parse_filament_weights()?;
                let sum: f64 = w.as_array()?.iter().filter_map(|v| v.as_f64()).sum();
                fval(Some(sum))
            }
            Kind::Simplify3D => fval(regex_find_float(r";\s+(?:Plastic\sweight|Material\sWeight):\s(%F)\sg", f)),
            Kind::IdeaMaker => {
                let pi = std::f64::consts::PI;
                let length = regex_find_floats(r";Material.\d\sUsed:\s+(%F)", f);
                let diameter = regex_find_floats(r";Filament\sDiameter\s.\d:\s+(%F)", h);
                let density = regex_find_floats(r";Filament\sDensity\s.\d:\s+(%F)", h);
                if length.len() == density.len() && density.len() == diameter.len() {
                    let total: f64 = (0..length.len())
                        .map(|i| pi / 4.0 * diameter[i].powi(2) * length[i] * density[i] / 1e6)
                        .sum();
                    // python's sum() of an empty list is the int 0
                    if length.is_empty() {
                        return Some(json!(0));
                    }
                    return fval(Some(total));
                }
                None
            }
            Kind::IceSL => fval(regex_find_float(r";\sfilament_used_g\s:\s+(%F)", h)),
            _ => None,
        }
    }

    fn footer_string(&self, key: &str) -> Option<Value> {
        if self.kind.is_prusa() {
            return sval(regex_find_string(&format!("; {} = (%S)", key), &self.footer_data));
        }
        None
    }

    fn parse_thumbnails(&self) -> Option<Value> {
        if matches!(self.kind, Kind::Slic3rPE | Kind::Slic3r) {
            return None;
        }
        let base = self.base_thumbnails();
        if self.kind == Kind::Cura && base.is_none() {
            return self.cura_ufp_thumbnails();
        }
        base
    }

    fn thumb_dir(&self) -> PathBuf {
        self.path.parent().unwrap_or(Path::new(".")).join(".thumbs")
    }

    fn thumb_base(&self) -> String {
        let name = self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        crate::pycompat::posixpath::splitext(&name).0.to_string()
    }

    fn base_thumbnails(&self) -> Option<Value> {
        let thumb_dir = self.thumb_dir();
        if !thumb_dir.exists() && std::fs::create_dir(&thumb_dir).is_err() {
            logi(&format!("Unable to create thumb dir: {}", thumb_dir.display()));
            return None;
        }
        let thumb_base = self.thumb_base();
        let pattern = FRegex::new(r"(thumbnail(?:_[A-Za-z0-9]+)?) begin([;/\+=\w\s]+?); \1 end").ok()?;
        let mut parsed: Vec<Map<String, Value>> = Vec::new();
        let mut has_miniature = false;
        for cap in pattern.captures_iter(&self.file_data) {
            let cap = match cap {
                Ok(c) => c,
                Err(_) => break,
            };
            let tag = cap.get(1).unwrap().as_str();
            let ext = tag.split_once('_').map(|(_, e)| e.to_lowercase()).filter(|e| !e.is_empty()).unwrap_or_else(|| "png".into());
            if !SUPPORTED_THUMB_FORMATS.contains(&ext.as_str()) {
                logi(&format!("Unsupported thumbnail extension: {}", ext));
                continue;
            }
            let body = cap.get(2).unwrap().as_str().replace("; ", "");
            let lines: Vec<&str> = re(r"\r?\n").split(&body).collect();
            let info = regex_find_ints(r"(%D)", lines[0]);
            let data: String = if lines.len() > 2 { lines[1..lines.len() - 1].concat() } else { String::new() };
            if info.len() != 3 {
                logi(&format!("MetadataError: Error parsing thumbnail header: {}", lines[0]));
                continue;
            }
            if data.len() as i64 != info[2] {
                logi(&format!("MetadataError: Thumbnail Size Mismatch: detected {}, actual {}", info[2], data.len()));
                continue;
            }
            let dest_ext = if ext == "qoi" { "png".to_string() } else { ext.clone() };
            let thumb_name = format!("{}-{}x{}.{}", thumb_base, info[0], info[1], dest_ext);
            let thumb_path = thumb_dir.join(&thumb_name);
            let rel = format!(".thumbs/{}", thumb_name);
            let decoded = match b64decode_lenient(&data) {
                Some(d) => d,
                None => continue,
            };
            if dest_ext != ext {
                match image::load_from_memory(&decoded) {
                    Ok(img) => {
                        if save_png(&img, &thumb_path).is_err() {
                            continue;
                        }
                    }
                    Err(e) => {
                        logi(&e.to_string());
                        continue;
                    }
                }
            } else if std::fs::write(&thumb_path, &decoded).is_err() {
                continue;
            }
            let size = std::fs::metadata(&thumb_path).map(|m| m.len()).unwrap_or(0);
            let mut m = Map::new();
            m.insert("width".into(), json!(info[0]));
            m.insert("height".into(), json!(info[1]));
            m.insert("size".into(), json!(size));
            m.insert("relative_path".into(), json!(rel));
            parsed.push(m);
            if info[0] == 32 && info[1] == 32 {
                has_miniature = true;
            }
        }
        if parsed.is_empty() {
            return None;
        }
        if !has_miniature {
            let mut largest = 0usize;
            for (i, item) in parsed.iter().enumerate() {
                if item["size"].as_u64() > parsed[largest]["size"].as_u64() {
                    largest = i;
                }
            }
            let full_name = parsed[largest]["relative_path"].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
            let src = thumb_dir.join(&full_name);
            let small_name = format!("{}-32x32.png", thumb_base);
            let small_path = thumb_dir.join(&small_name);
            match make_miniature(&src, &small_path) {
                Ok((w, h)) => {
                    let mut m = Map::new();
                    m.insert("width".into(), json!(w));
                    m.insert("height".into(), json!(h));
                    m.insert("size".into(), json!(std::fs::metadata(&small_path).map(|m| m.len()).unwrap_or(0)));
                    m.insert("relative_path".into(), json!(format!(".thumbs/{}", small_name)));
                    parsed.insert(0, m);
                }
                Err(e) => logi(&e),
            }
        }
        Some(Value::Array(parsed.into_iter().map(Value::Object).collect()))
    }

    fn cura_ufp_thumbnails(&self) -> Option<Value> {
        let thumb_dir = self.thumb_dir();
        let base = self.thumb_base();
        let thumb_path = thumb_dir.join(format!("{}.png", base));
        let small_path = thumb_dir.join(format!("{}-32x32.png", base));
        if !thumb_path.is_file() {
            return None;
        }
        let img = match image::open(&thumb_path) {
            Ok(i) => i,
            Err(e) => {
                logi(&e.to_string());
                return None;
            }
        };
        let mut thumbs = vec![json!({
            "width": img.width(), "height": img.height(),
            "size": std::fs::metadata(&thumb_path).map(|m| m.len()).unwrap_or(0),
            "relative_path": format!(".thumbs/{}.png", base),
        })];
        match make_miniature(&thumb_path, &small_path) {
            Ok((w, h)) => thumbs.insert(
                0,
                json!({
                    "width": w, "height": h,
                    "size": std::fs::metadata(&small_path).map(|m| m.len()).unwrap_or(0),
                    "relative_path": format!(".thumbs/{}-32x32.png", base),
                }),
            ),
            Err(e) => {
                logi(&e);
                return None;
            }
        }
        Some(Value::Array(thumbs))
    }

    pub fn run_parsers(&mut self) -> Map<String, Value> {
        let mut result = Map::new();
        macro_rules! run {
            ($name:literal, $e:expr) => {
                if let Some(v) = $e {
                    result.insert($name.to_string(), v);
                }
            };
        }
        run!("gcode_start_byte", self.parse_gcode_start_byte());
        run!("gcode_end_byte", self.parse_gcode_end_byte());
        run!("layer_count", self.parse_layer_count());
        run!("object_height", self.parse_object_height());
        run!("estimated_time", self.parse_estimated_time());
        run!("nozzle_diameter", self.parse_nozzle_diameter());
        run!("layer_height", self.parse_layer_height());
        run!("first_layer_height", self.parse_first_layer_height());
        run!("first_layer_extr_temp", self.parse_first_layer_extr_temp());
        run!("first_layer_bed_temp", self.parse_first_layer_bed_temp());
        run!("chamber_temp", self.parse_chamber_temp());
        run!("filament_name", self.parse_filament_name());
        run!("filament_type", self.parse_filament_type());
        run!("filament_colors", self.parse_filament_colors());
        run!("filament_change_count", self.parse_filament_change_count());
        run!("extruder_colors", self.parse_extruder_colors());
        run!("filament_temps", self.parse_filament_temps());
        run!("referenced_tools", self.parse_referenced_tools());
        run!("mmu_print", self.parse_mmu_print());
        run!("filament_total", self.parse_filament_total());
        run!("filament_weight_total", self.parse_filament_weight_total());
        run!("filament_weights", self.parse_filament_weights());
        run!("printer_vendor", self.footer_string("printer_vendor"));
        run!("printer_model", self.footer_string("printer_model"));
        run!("printer_variant", self.footer_string("printer_variant"));
        run!("profile_version", self.footer_string("profile_version"));
        run!("thumbnails", self.parse_thumbnails());
        result
    }
}

/// python json.dumps escapes non-ascii by default
/// json.dumps(str) with python's default ensure_ascii=True
fn py_json_ascii(s: &str) -> String {
    let encoded = serde_json::to_string(s).unwrap_or_default();
    let mut out = String::with_capacity(encoded.len());
    for c in encoded.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{:04x}", u));
            }
        }
    }
    out
}

fn b64decode_lenient(data: &str) -> Option<Vec<u8>> {
    let clean: String = data.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=').collect();
    let trimmed = clean.trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed).ok()
}

/// PIL's Image.thumbnail((32, 32)) followed by a PNG save
fn make_miniature(src: &Path, dest: &Path) -> Result<(u32, u32), String> {
    let img = image::open(src).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let (mut x, mut y) = (32u32, 32u32);
    if !(x >= w && y >= h) {
        let aspect = w as f64 / h as f64;
        let round_aspect = |number: f64, key: &dyn Fn(f64) -> f64| -> u32 {
            let lo = number.floor();
            let hi = number.ceil();
            let best = if key(hi) < key(lo) { hi } else { lo };
            best.max(1.0) as u32
        };
        if x as f64 / y as f64 >= aspect {
            let yy = y as f64;
            x = round_aspect(yy * aspect, &|n| (aspect - n / yy).abs());
        } else {
            let xx = x as f64;
            y = round_aspect(xx / aspect, &|n| if n == 0.0 { 0.0 } else { (aspect - xx / n).abs() });
        }
    } else {
        x = w;
        y = h;
    }
    let out = if x == w && y == h {
        img
    } else if img.color().has_alpha() {
        // PIL resizes RGBA images in premultiplied form (RGBa)
        let mut pm = img.to_rgba8();
        for p in pm.pixels_mut() {
            let a = p[3] as u32;
            for c in 0..3 {
                p[c] = ((p[c] as u32 * a + 127) / 255) as u8;
            }
        }
        let mut r = image::imageops::resize(&pm, x, y, image::imageops::FilterType::CatmullRom);
        for p in r.pixels_mut() {
            let a = p[3] as u32;
            for c in 0..3 {
                p[c] = if a == 0 { 0 } else { ((p[c] as u32 * 255 + a / 2) / a).min(255) as u8 };
            }
        }
        image::DynamicImage::ImageRgba8(r)
    } else {
        img.resize_exact(x, y, image::imageops::FilterType::CatmullRom)
    };
    save_png(&out, dest)?;
    Ok((out.width(), out.height()))
}

/// PIL's PNG save: zlib default level with adaptive row filters
fn save_png(img: &image::DynamicImage, dest: &Path) -> Result<(), String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let enc = PngEncoder::new_with_quality(std::io::BufWriter::new(f), CompressionType::Default, FilterType::Adaptive);
    img.write_with_encoder(enc).map_err(|e| e.to_string())
}

// ***** Processors *****

fn process_objects(file_path: &Path, slicer: &mut Slicer) -> bool {
    if !slicer.has_objects() {
        return false;
    }
    let proc = if slicer.has_m486_objects {
        "preprocess_m486"
    } else if slicer.kind.is_prusa() {
        "preprocess_slicer"
    } else if slicer.kind == Kind::Cura {
        "preprocess_cura"
    } else if slicer.kind == Kind::IdeaMaker {
        "preprocess_ideamaker"
    } else {
        logi(&format!("Object Processing Failed, slicer {}not supported", slicer.slicer_name));
        return false;
    };
    let fname = file_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    logi(&format!("Performing Object Processing on file: {}, sliced by {}", fname, slicer.slicer_name));
    let result = cancellation::read_text(file_path).and_then(|text| {
        let lines = cancellation::split_lines(&text);
        match proc {
            "preprocess_m486" => cancellation::preprocess_m486(&lines),
            "preprocess_slicer" => cancellation::preprocess_slicer(&lines),
            "preprocess_cura" => cancellation::preprocess_cura(&lines),
            _ => cancellation::preprocess_ideamaker(&lines),
        }
    });
    let out = match result {
        Ok(o) => o,
        Err(e) => {
            logi(&format!("Object processing failed: {}", e));
            return false;
        }
    };
    // written to a temporary directory, then moved over the original
    let tmp_dir = std::env::temp_dir().join(format!("ppc-{}", uuid::Uuid::new_v4().simple()));
    let tmp_file = tmp_dir.join(&fname);
    let res = std::fs::create_dir(&tmp_dir).and_then(|_| std::fs::write(&tmp_file, out.as_bytes()));
    if let Err(e) = res {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        logi(&format!("Object processing failed: {}", e));
        return false;
    }
    let mut dest = file_path.to_path_buf();
    if dest.is_symlink() {
        dest = std::fs::canonicalize(&dest).unwrap_or(dest);
    }
    let moved = crate::pycompat::shutil::shutil_move(&tmp_file.to_string_lossy(), &dest.to_string_lossy());
    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Err(e) = moved {
        logi(&format!("Object processing failed: {}", e));
        return false;
    }
    true
}

fn run_gcode_processors(path: &Path, slicer: &mut Slicer, processors: &[Value], check_objects: bool) -> (Vec<String>, bool) {
    let mut reload = false;
    let mut finished = Vec::new();
    let short = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut all: Vec<Value> = processors.to_vec();
    all.push(json!({
        "name": "preprocess_cancellation",
        "command": "__builtin_process_objects__",
        "enabled": check_objects,
        "ident": {"regex": PPC_REGEX, "location": "header"},
    }));
    for cfg in all {
        let name = cfg.get("name").map(crate::pycompat::pyfmt::py_str).unwrap_or_else(|| "Unknown".into());
        let version = cfg.get("version").map(crate::pycompat::pyfmt::py_str).unwrap_or_else(|| "v?".into());
        if let Some(ident) = cfg.get("ident").and_then(|i| i.as_object()).filter(|i| !i.is_empty()) {
            let regex = ident.get("regex").map(crate::pycompat::pyfmt::py_str).unwrap_or_default();
            let loc = ident.get("location").map(crate::pycompat::pyfmt::py_str).unwrap_or_default();
            if let Some(data) = slicer.check_gcode_processor(&regex, &loc) {
                let ver = data.get("version").map(crate::pycompat::pyfmt::py_str).unwrap_or_else(|| "v?".into());
                logi(&format!("File {} previously processed by {} {}", short, name, ver));
                finished.push(name);
                continue;
            }
        }
        if !cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
            logi(&format!("Processor {} is disabled", name));
            continue;
        }
        let command = cfg.get("command").cloned().unwrap_or(Value::Null);
        if command.as_str() == Some("__builtin_process_objects__") {
            if process_objects(path, slicer) {
                finished.push(name);
                reload = true;
            }
            continue;
        }
        let mut args: Vec<String> = match &command {
            // shlex's ValueError: the processor fails, as in metadata.py
            Value::String(s) => match crate::pycompat::shlex::shlex_split(s) {
                Ok(a) => a,
                Err(_) => {
                    logi(&format!("Processor {} failed with error", name));
                    continue;
                }
            },
            Value::Array(a) => a.iter().map(crate::pycompat::pyfmt::py_str).collect(),
            _ => {
                logi(&format!("Processor {} failed with error", name));
                continue;
            }
        };
        for a in args.iter_mut() {
            if a == "{gcode_file_path}" {
                *a = path.to_string_lossy().to_string();
            }
        }
        let timeout = cfg.get("timeout").and_then(|v| v.as_f64()).unwrap_or(120.0);
        if args.is_empty() || timeout <= 0.0 {
            logi(&format!("Processor {} failed with error", name));
            continue;
        }
        logi(&format!("Running processor {} {} on file {}...", name, version, short));
        let child = std::process::Command::new(&args[0])
            .args(&args[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                logi(&format!("Processor {} failed with error", name));
                logi(&e.to_string());
                continue;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = match rx.recv_timeout(std::time::Duration::from_secs_f64(timeout)) {
            Ok(Ok(o)) => o,
            _ => {
                logi(&format!("Processor {} failed with error", name));
                continue;
            }
        };
        if !out.status.success() {
            logi(&format!(
                "File processor {} failed with code {}",
                name,
                out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "None".into())
            ));
            let so = String::from_utf8_lossy(&out.stdout);
            let se = String::from_utf8_lossy(&out.stderr);
            if !so.is_empty() {
                logi(&so);
            }
            if !se.is_empty() {
                logi(&se);
            }
        } else {
            logi(&format!("File processor {} successfully complete", name));
            finished.push(name);
            reload = true;
        }
    }
    (finished, reload)
}

fn extract_metadata(path: &Path, processors: &[Value], check_objects: bool) -> std::io::Result<Map<String, Value>> {
    let mut slicer = Slicer::from_file(path)?;
    let (proc_list, reload) = run_gcode_processors(path, &mut slicer, processors, check_objects);
    if reload {
        slicer = Slicer::from_file(path)?;
    }
    let meta = std::fs::metadata(path)?;
    let mtime = {
        use std::os::unix::fs::MetadataExt;
        meta.mtime() as f64 + meta.mtime_nsec() as f64 / 1e9
    };
    let mut m = Map::new();
    m.insert("size".into(), json!(meta.len()));
    m.insert("modified".into(), crate::pycompat::pyfmt::float_value(mtime));
    m.insert("uuid".into(), json!(uuid::Uuid::new_v4().to_string()));
    m.insert("file_processors".into(), json!(proc_list));
    m.insert("slicer".into(), json!(slicer.slicer_name));
    m.insert("slicer_version".into(), json!(slicer.slicer_version));
    for (k, v) in slicer.run_parsers() {
        m.insert(k, v);
    }
    Ok(m)
}

fn extract_ufp(ufp_path: &Path, dest_path: &Path) -> Result<(), String> {
    if !ufp_path.is_file() {
        logi(&format!("UFP file Not Found: {}", ufp_path.display()));
        return Err("missing".into());
    }
    let name = dest_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let thumb_name = format!("{}.png", crate::pycompat::posixpath::splitext(&name).0.to_string());
    let thumb_dir = dest_path.parent().unwrap_or(Path::new(".")).join(".thumbs");
    let f = std::fs::File::open(ufp_path).map_err(|e| e.to_string())?;
    let mut zf = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    // zipfile.getinfo() matches the stored name exactly
    let find = |zf: &zip::ZipArchive<std::fs::File>, name: &str| zf.index_for_name(name);
    let mut model = Vec::new();
    {
        let idx = find(&zf, UFP_MODEL_PATH).ok_or_else(|| format!("There is no item named '{}' in the archive", UFP_MODEL_PATH))?;
        let mut entry = zf.by_index(idx).map_err(|e| e.to_string())?;
        entry.read_to_end(&mut model).map_err(|e| e.to_string())?;
    }
    let mut thumb = None;
    if let Some(idx) = find(&zf, UFP_THUMB_PATH) {
        if let Ok(mut entry) = zf.by_index(idx) {
            let mut t = Vec::new();
            if entry.read_to_end(&mut t).is_ok() {
                thumb = Some(t);
            }
        }
    }
    let mut dest = dest_path.to_path_buf();
    if dest.is_symlink() {
        dest = std::fs::canonicalize(&dest).unwrap_or(dest);
    }
    std::fs::write(&dest, &model).map_err(|e| e.to_string())?;
    if let Some(t) = thumb {
        if !thumb_dir.exists() {
            let _ = std::fs::create_dir(&thumb_dir);
        }
        std::fs::write(thumb_dir.join(&thumb_name), t).map_err(|e| e.to_string())?;
    }
    if std::fs::remove_file(ufp_path).is_err() {
        logi(&format!("Error removing ufp file: {}", ufp_path.display()));
    }
    Ok(())
}

/// Entry point for the metadata subprocess.  Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let cfg_path = match args.iter().position(|a| a == "-c" || a == "--config").and_then(|i| args.get(i + 1)) {
        Some(p) => p.clone(),
        None => {
            logi("The '--config' (-c) option must be specified");
            return 255;
        }
    };
    let config: Value = match std::fs::read_to_string(&cfg_path).ok().and_then(|d| serde_json::from_str(&d).ok()) {
        Some(c) => c,
        None => {
            logi(&format!("Unable to read configuration file {}", cfg_path));
            return 255;
        }
    };
    let filename = match config.get("filename").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => {
            logi("The 'filename' field must be present in the configuration");
            return 255;
        }
    };
    let gc_path = config.get("gcode_dir").and_then(|v| v.as_str()).unwrap_or(".").to_string();
    let file_path = Path::new(&gc_path).join(&filename);
    let processors: Vec<Value> = config.get("processors").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let check_objects = config.get("check_objects").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(ufp) = config.get("ufp_path").and_then(|v| v.as_str()) {
        if let Err(e) = extract_ufp(Path::new(ufp), &file_path) {
            logi(&e);
            return 255;
        }
    }
    if !file_path.is_file() {
        logi(&format!("File Not Found: {}", file_path.display()));
        return 255;
    }
    let metadata = match extract_metadata(&file_path, &processors, check_objects) {
        Ok(m) => m,
        Err(e) => {
            logi(&e.to_string());
            return 255;
        }
    };
    let out = json!({"file": filename, "metadata": metadata});
    print!("{}", out);
    0
}

#[cfg(test)]
mod ascii_tests {
    #[test]
    fn ensure_ascii() {
        assert_eq!(super::py_json_ascii("PLA \"x\" é😀"), r#""PLA \"x\" \u00e9\ud83d\ude00""#);
    }
}
