// pathlib's PurePath.suffix and .stem, of a path's final component (name).
// Path::extension and Path::file_stem differ for a name ending in '.'.

/// PurePath.suffix
pub fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if 0 < i && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// PurePath(p).suffix
pub fn path_suffix(p: &std::path::Path) -> String {
    suffix(&p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()).to_string()
}

/// PurePath.stem
pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if 0 < i && i < name.len() - 1 => &name[..i],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    // CPython 3.12.3's PurePosixPath(name).suffix and .stem
    #[test]
    fn cpython() {
        for (name, suffix, stem) in [
            ("a.", "", "a."),
            ("..gcode", ".gcode", "."),
            (".gcode", "", ".gcode"),
            ("a.b.c", ".c", "a.b"),
            ("a..", "", "a.."),
            ("..", "", ".."),
            ("", "", ""),
            ("x.gcode", ".gcode", "x"),
        ] {
            assert_eq!(super::suffix(name), suffix, "suffix {name:?}");
            assert_eq!(super::stem(name), stem, "stem {name:?}");
        }
    }
}
