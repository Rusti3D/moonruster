// Port of the preprocess_cancellation package (v0.2.1, which reports itself
// as v0.2.0) used by moonraker's metadata parser to add EXCLUDE_OBJECT
// markers.  Moonraker does not install shapely, so object hulls are the
// bounding boxes of SimpleHullTracker.
//
// Errors carry the message of the python exception the original raises,
// in which case moonraker leaves the file untouched.

use crate::pycompat::pyfmt::{py_float, py_float_repr, py_int, py_repr};
use indexmap::IndexMap;
use serde_json::Value;

const HEADER_MARKER: &str = "; Pre-Processed for Cancel-Object support by preprocess_cancellation v0.2.0\n";

type Res<T> = Result<T, String>;

#[derive(Default)]
struct Hull {
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    count: usize,
    xsum: f64,
    ysum: f64,
}

impl Hull {
    fn new() -> Self {
        Hull { xmin: 999999.0, ymin: 999999.0, xmax: -999999.0, ymax: -999999.0, ..Default::default() }
    }
    fn add_point(&mut self, x: f64, y: f64) {
        self.xsum += x;
        self.ysum += y;
        self.count += 1;
        if x < self.xmin {
            self.xmin = x;
        }
        if y < self.ymin {
            self.ymin = y;
        }
        if x > self.xmax {
            self.xmax = x;
        }
        if y > self.ymax {
            self.ymax = y;
        }
    }
    fn center(&self) -> Option<(f64, f64)> {
        if self.count == 0 {
            return None;
        }
        Some((self.xsum / self.count as f64, self.ysum / self.count as f64))
    }
    fn exterior(&self) -> Option<[(f64, f64); 4]> {
        if self.count == 0 {
            return None;
        }
        Some([(self.xmin, self.ymin), (self.xmin, self.ymax), (self.xmax, self.ymax), (self.xmax, self.ymin)])
    }
}

struct Known {
    name: String,
    hull: Hull,
}

/// json.dumps() of a float
fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        py_float_repr(f)
    }
}

/// "{:0.3f}".format(f)
fn fmt3(f: f64) -> String {
    if f.is_nan() {
        "nan".into()
    } else {
        format!("{:.3}", f)
    }
}

fn define_object(out: &mut String, name: &str, hull: &Hull) {
    out.push_str(&format!("EXCLUDE_OBJECT_DEFINE NAME={}", name));
    if let Some((x, y)) = hull.center() {
        out.push_str(&format!(" CENTER={},{}", fmt3(x), fmt3(y)));
    }
    if let Some(poly) = hull.exterior() {
        let pts: Vec<String> = poly.iter().map(|(x, y)| format!("[{},{}]", json_float(*x), json_float(*y))).collect();
        out.push_str(&format!(" POLYGON=[{}]", pts.join(",")));
    }
    out.push('\n');
}

fn header(out: &mut String, count: usize) {
    out.push_str("\n\n");
    out.push_str(HEADER_MARKER);
    out.push_str(&format!("; {} known objects\n", count));
}

/// re.sub(r"\W+", "_", id).strip("_")
fn clean_id(id: &str) -> String {
    let re = crate::pycompat::re::compile(r"\W+");
    re.replace_all(id, "_").trim_matches('_').to_string()
}

/// parse_gcode(): command and parameters of a gcode line
fn parse_gcode(line: &str) -> Res<IndexMap<String, String>> {
    let code = line.split(';').next().unwrap_or("");
    let mut parts = code.split_whitespace();
    if parts.next().is_none() {
        return Err("not enough values to unpack (expected at least 1, got 0)".into());
    }
    let mut params = IndexMap::new();
    for p in parts {
        if p.contains('=') {
            // dict(zip(param.split("=", 1))) builds 1-tuples
            return Err("dictionary update sequence element #0 has length 1; 2 is required".into());
        }
        let mut chars = p.chars();
        let first = chars.next().unwrap();
        params.insert(first.to_uppercase().collect(), chars.as_str().to_string());
    }
    Ok(params)
}

fn to_float(s: &str) -> Res<f64> {
    py_float(s).ok_or_else(|| format!("could not convert string to float: {}", py_repr(&Value::String(s.into()))))
}

fn key_error(k: &str) -> String {
    py_repr(&Value::String(k.into()))
}

/// The hull tracking shared by all processors
fn track(hull: &mut Hull, line: &str) -> Res<()> {
    if !line.trim().to_lowercase().starts_with('g') {
        return Ok(());
    }
    let params = parse_gcode(line)?;
    let e = match params.get("E") {
        Some(v) => to_float(v)?,
        None => -1.0,
    };
    if e > 0.0 && params.contains_key("X") && params.contains_key("Y") {
        let x = to_float(&params["X"])?;
        let y = to_float(&params["Y"])?;
        hull.add_point(x, y);
    }
    Ok(())
}

fn is_code_line(line: &str) -> bool {
    !line.trim().is_empty() && !line.starts_with(';')
}

/// line.split(sep)[1]
fn split_second<'a>(line: &'a str, sep: &str) -> Res<&'a str> {
    line.split(sep).nth(1).ok_or_else(|| "list index out of range".to_string())
}

pub fn preprocess_m486(lines: &[&str]) -> Res<String> {
    let mut known: IndexMap<String, Known> = IndexMap::new();
    let mut current: Option<String> = None;
    for line in lines {
        if line.starts_with("M486") {
            let params = parse_gcode(line)?;
            if let Some(t) = params.get("T") {
                let n = py_int(t).ok_or_else(|| format!("invalid literal for int() with base 10: {}", py_repr(&Value::String(t.clone()))))?;
                for i in -1..n {
                    known.insert(i.to_string(), Known { name: i.to_string(), hull: Hull::new() });
                }
            } else if let Some(s) = params.get("S") {
                if !known.contains_key(s) {
                    return Err(key_error(s));
                }
                current = Some(s.clone());
            }
        }
        if let Some(c) = &current {
            track(&mut known.get_mut(c).unwrap().hull, line)?;
        }
    }
    let mut out = String::new();
    let mut cur_obj: Option<String> = None;
    for line in lines {
        if line.to_uppercase().starts_with("M486") {
            let params = parse_gcode(line)?;
            if params.contains_key("T") {
                header(&mut out, known.len());
                for k in known.values() {
                    if k.name == "-1" {
                        continue;
                    }
                    define_object(&mut out, &k.name, &k.hull);
                }
            }
            if let Some(s) = params.get("S") {
                if let Some(c) = cur_obj.take() {
                    out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
                }
                if s != "-1" {
                    let k = known.get(s).ok_or_else(|| key_error(s))?;
                    out.push_str(&format!("EXCLUDE_OBJECT_START NAME={}\n", k.name));
                    cur_obj = Some(k.name.clone());
                }
            }
            out.push_str("; ");
        }
        out.push_str(line);
    }
    Ok(out)
}

pub fn preprocess_cura(lines: &[&str]) -> Res<String> {
    let mut known: IndexMap<String, Known> = IndexMap::new();
    let mut current: Option<String> = None;
    let mut last_time_elapsed: Option<&str> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix(";MESH:") {
            let name = rest.trim();
            if name == "NONMESH" {
                continue;
            }
            if !known.contains_key(name) {
                known.insert(name.to_string(), Known { name: clean_id(name), hull: Hull::new() });
            }
            current = Some(name.to_string());
        }
        if let Some(c) = &current {
            track(&mut known.get_mut(c).unwrap().hull, line)?;
        }
        if line.starts_with(";TIME_ELAPSED:") {
            last_time_elapsed = Some(line);
        }
    }
    let mut out = String::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        idx += 1;
        if is_code_line(line) {
            header(&mut out, known.len());
            for k in known.values() {
                define_object(&mut out, &k.name, &k.hull);
            }
            out.push_str(line);
            break;
        }
        out.push_str(line);
    }
    let mut cur_obj: Option<String> = None;
    for line in &lines[idx..] {
        out.push_str(line);
        if let Some(rest) = line.strip_prefix(";MESH:") {
            if let Some(c) = cur_obj.take() {
                out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
            }
            let mesh = rest.trim();
            if mesh == "NONMESH" {
                continue;
            }
            let k = known.get(mesh).ok_or_else(|| key_error(mesh))?;
            out.push_str(&format!("EXCLUDE_OBJECT_START NAME={}\n", k.name));
            cur_obj = Some(k.name.clone());
        }
        if Some(*line) == last_time_elapsed {
            if let Some(c) = cur_obj.take() {
                out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
            }
        }
    }
    if let Some(c) = cur_obj {
        out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
    }
    Ok(out)
}

pub fn preprocess_slicer(lines: &[&str]) -> Res<String> {
    let mut known: IndexMap<String, Known> = IndexMap::new();
    let mut current: Option<String> = None;
    for line in lines {
        if line.starts_with("; printing object ") {
            let id = split_second(line, "printing object")?.trim();
            if !known.contains_key(id) {
                known.insert(id.to_string(), Known { name: clean_id(id), hull: Hull::new() });
            }
            current = Some(id.to_string());
        }
        if line.starts_with("; stop printing object ") {
            current = None;
        }
        if let Some(c) = &current {
            track(&mut known.get_mut(c).unwrap().hull, line)?;
        }
    }
    let mut out = String::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        idx += 1;
        if is_code_line(line) {
            header(&mut out, known.len());
            for k in known.values() {
                define_object(&mut out, &k.name, &k.hull);
            }
            out.push_str(line);
            break;
        }
        out.push_str(line);
    }
    for line in &lines[idx..] {
        out.push_str(line);
        if line.starts_with("; printing object ") {
            let id = split_second(line, "printing object")?.trim();
            let k = known.get(id).ok_or_else(|| key_error(id))?;
            out.push_str(&format!("EXCLUDE_OBJECT_START NAME={}\n", k.name));
        }
        if line.starts_with("; stop printing object ") {
            let id = split_second(line, "printing object")?.trim();
            let k = known.get(id).ok_or_else(|| key_error(id))?;
            out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", k.name));
        }
    }
    Ok(out)
}

pub fn preprocess_ideamaker(lines: &[&str]) -> Res<String> {
    let mut known: IndexMap<String, Known> = IndexMap::new();
    let mut current: Option<String> = None;
    let mut total_num: Option<i64> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with(";TOTAL_NUM:") {
            let v = split_second(line, ":")?.trim();
            total_num = Some(py_int(v).ok_or_else(|| format!("invalid literal for int() with base 10: {}", py_repr(&Value::String(v.into()))))?);
        }
        if line.starts_with(";PRINTING:") {
            let name = split_second(line, ":")?.trim();
            // next(infile)
            let id_line = *lines.get(i).ok_or_else(|| "generator raised StopIteration".to_string())?;
            i += 1;
            if !id_line.starts_with(";PRINTING_ID:") {
                return Err(String::new());
            }
            let id = split_second(id_line, ":")?.trim();
            if id == "-1" {
                continue;
            }
            if !known.contains_key(id) {
                known.insert(id.to_string(), Known { name: clean_id(name), hull: Hull::new() });
            }
            current = Some(id.to_string());
        }
        if let Some(c) = &current {
            track(&mut known.get_mut(c).unwrap().hull, line)?;
        }
    }
    let mut out = String::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        idx += 1;
        out.push_str(line);
        if is_code_line(line) {
            break;
        }
    }
    let total = total_num.ok_or_else(|| "cannot access local variable 'total_num' where it is not associated with a value".to_string())?;
    if total != known.len() as i64 {
        return Err(String::new());
    }
    header(&mut out, total as usize);
    for k in known.values() {
        define_object(&mut out, &k.name, &k.hull);
    }
    out.push_str("\n\n");
    let mut cur_obj: Option<String> = None;
    for line in &lines[idx..] {
        out.push_str(line);
        if line.starts_with(";PRINTING_ID:") {
            let pid = split_second(line, ":")?.trim();
            if let Some(c) = cur_obj.take() {
                out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
            }
            if pid == "-1" {
                continue;
            }
            let k = known.get(pid).ok_or_else(|| key_error(pid))?;
            out.push_str(&format!("EXCLUDE_OBJECT_START NAME={}\n", k.name));
            cur_obj = Some(k.name.clone());
        }
        if *line == ";REMAINING_TIME: 0\n" {
            if let Some(c) = cur_obj.take() {
                out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
            }
        }
    }
    if let Some(c) = cur_obj {
        out.push_str(&format!("EXCLUDE_OBJECT_END NAME={}\n", c));
    }
    Ok(out)
}

/// Reads a file the way python's open(path, 'r') does: strict utf-8 and
/// universal newlines.  Returns the text, lines keep their "\n".
pub fn read_text(path: &std::path::Path) -> Res<String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(data).map_err(|e| {
        let pos = e.utf8_error().valid_up_to();
        format!("'utf-8' codec can't decode byte {:#04x} in position {}: invalid start byte", e.as_bytes()[pos], pos)
    })?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slicer_markers() {
        let src = "; generated by PrusaSlicer 2.7.1\n\nG28\n; printing object cube.stl id:0 copy 0\nG1 X10 Y10 E1\nG1 X20.5 Y12 E1\n; stop printing object cube.stl id:0 copy 0\n";
        let out = preprocess_slicer(&split_lines(src)).unwrap();
        assert!(out.contains("; 1 known objects\nEXCLUDE_OBJECT_DEFINE NAME=cube_stl_id_0_copy_0 CENTER=15.250,11.000 POLYGON=[[10.0,10.0],[10.0,12.0],[20.5,12.0],[20.5,10.0]]\nG28\n"));
        assert!(out.ends_with("; stop printing object cube.stl id:0 copy 0\nEXCLUDE_OBJECT_END NAME=cube_stl_id_0_copy_0\n"));
        // a KEY=value parameter on a tracked move raises in the original
        let bad = "; printing object a\nG1 X1 Y1 E1 FOO=1\n";
        assert!(preprocess_slicer(&split_lines(bad)).is_err());
    }
}
