// Python's mimetypes.guess_type, as tornado's StaticFileHandler uses it for
// Content-Type (Moonraker's file and static handlers): CPython's default
// tables (from CPython 3.15.0, mimetypes._types_map_default / _encodings_map_default
// / _suffix_map_default), then the mime.types files mimetypes.init() reads,
// in its order (a later entry replaces an earlier one), then the types
// Moonraker adds (application.py: .log, .gcode, .cfg as text/plain).
// The files are scanned per extension, the answer kept: no table of the
// whole files in memory.
use std::cell::RefCell;
use std::collections::HashMap;

/// mimetypes.knownfiles
const KNOWN_FILES: [&str; 9] = [
    "/etc/mime.types",
    "/etc/httpd/mime.types",
    "/etc/httpd/conf/mime.types",
    "/etc/apache/mime.types",
    "/etc/apache2/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/lib/netscape/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/etc/mime.types",
];

/// _types_map_default (sorted by extension)
const DEFAULT_TYPES: &[(&str, &str)] = &[
    (".3g2", "audio/3gpp2"),
    (".3gp", "audio/3gpp"),
    (".3gpp", "audio/3gpp"),
    (".3gpp2", "audio/3gpp2"),
    (".7z", "application/x-7z-compressed"),
    (".a", "application/octet-stream"),
    (".aac", "audio/aac"),
    (".adts", "audio/aac"),
    (".ai", "application/pdf"),
    (".aif", "audio/x-aiff"),
    (".aifc", "audio/x-aiff"),
    (".aiff", "audio/x-aiff"),
    (".ass", "audio/aac"),
    (".au", "audio/basic"),
    (".avi", "video/vnd.avi"),
    (".avif", "image/avif"),
    (".bat", "text/plain"),
    (".bcpio", "application/x-bcpio"),
    (".bin", "application/octet-stream"),
    (".bmp", "image/bmp"),
    (".c", "text/plain"),
    (".cab", "application/vnd.ms-cab-compressed"),
    (".cdf", "application/x-netcdf"),
    (".chm", "application/vnd.ms-htmlhelp"),
    (".cjs", "application/node"),
    (".cpio", "application/x-cpio"),
    (".csh", "application/x-csh"),
    (".css", "text/css"),
    (".csv", "text/csv"),
    (".dcm", "application/dicom"),
    (".deb", "application/x-debian-package"),
    (".dll", "application/vnd.microsoft.portable-executable"),
    (".doc", "application/msword"),
    (".docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    (".dot", "application/msword"),
    (".dvi", "application/x-dvi"),
    (".efi", "application/efi"),
    (".emf", "image/emf"),
    (".eml", "message/rfc822"),
    (".eot", "application/vnd.ms-fontobject"),
    (".eps", "application/postscript"),
    (".epub", "application/epub+zip"),
    (".etx", "text/x-setext"),
    (".exe", "application/vnd.microsoft.portable-executable"),
    (".fits", "image/fits"),
    (".flac", "audio/flac"),
    (".g3", "image/g3fax"),
    (".gif", "image/gif"),
    (".glb", "model/gltf-binary"),
    (".gltf", "model/gltf+json"),
    (".gtar", "application/x-gtar"),
    (".gz", "application/gzip"),
    (".h", "text/plain"),
    (".h5", "application/x-hdf5"),
    (".hdf", "application/x-hdf"),
    (".heic", "image/heic"),
    (".heif", "image/heif"),
    (".hjif", "haptics/hjif"),
    (".hmpg", "haptics/hmpg"),
    (".htm", "text/html"),
    (".html", "text/html"),
    (".ico", "image/vnd.microsoft.icon"),
    (".ief", "image/ief"),
    (".ivs", "haptics/ivs"),
    (".jp2", "image/jp2"),
    (".jpe", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".jpm", "image/jpm"),
    (".jpx", "image/jpx"),
    (".js", "text/javascript"),
    (".json", "application/json"),
    (".jxl", "image/jxl"),
    (".ksh", "text/plain"),
    (".latex", "application/x-latex"),
    (".loas", "audio/aac"),
    (".m1v", "video/mpeg"),
    (".m3u", "application/vnd.apple.mpegurl"),
    (".m3u8", "application/vnd.apple.mpegurl"),
    (".m4a", "audio/mp4"),
    (".m4v", "video/x-m4v"),
    (".man", "application/x-troff-man"),
    (".markdown", "text/markdown"),
    (".md", "text/markdown"),
    (".me", "application/x-troff-me"),
    (".mht", "message/rfc822"),
    (".mhtml", "message/rfc822"),
    (".mif", "application/x-mif"),
    (".mjs", "text/javascript"),
    (".mk3d", "video/matroska-3d"),
    (".mka", "audio/matroska"),
    (".mkv", "video/matroska"),
    (".mov", "video/quicktime"),
    (".movie", "video/x-sgi-movie"),
    (".mp2", "audio/mpeg"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".mpa", "video/mpeg"),
    (".mpe", "video/mpeg"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
    (".ms", "application/x-troff-ms"),
    (".n3", "text/n3"),
    (".nc", "application/x-netcdf"),
    (".nq", "application/n-quads"),
    (".nt", "application/n-triples"),
    (".nws", "message/rfc822"),
    (".o", "application/octet-stream"),
    (".obj", "application/octet-stream"),
    (".oda", "application/oda"),
    (".odg", "application/vnd.oasis.opendocument.graphics"),
    (".odp", "application/vnd.oasis.opendocument.presentation"),
    (".ods", "application/vnd.oasis.opendocument.spreadsheet"),
    (".odt", "application/vnd.oasis.opendocument.text"),
    (".ogg", "audio/ogg"),
    (".ogv", "video/ogg"),
    (".ogx", "application/ogg"),
    (".opus", "audio/opus"),
    (".otf", "font/otf"),
    (".p12", "application/x-pkcs12"),
    (".p7c", "application/pkcs7-mime"),
    (".pbm", "image/x-portable-bitmap"),
    (".pdf", "application/pdf"),
    (".pfx", "application/x-pkcs12"),
    (".pgm", "image/x-portable-graymap"),
    (".php", "application/x-httpd-php"),
    (".pl", "text/plain"),
    (".png", "image/png"),
    (".pnm", "image/x-portable-anymap"),
    (".pot", "application/vnd.ms-powerpoint"),
    (".ppa", "application/vnd.ms-powerpoint"),
    (".ppm", "image/x-portable-pixmap"),
    (".pps", "application/vnd.ms-powerpoint"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (".pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    (".ps", "application/postscript"),
    (".pwz", "application/vnd.ms-powerpoint"),
    (".py", "text/x-python"),
    (".pyc", "application/x-python-code"),
    (".pyo", "application/x-python-code"),
    (".qt", "video/quicktime"),
    (".ra", "audio/x-pn-realaudio"),
    (".ram", "application/x-pn-realaudio"),
    (".rar", "application/vnd.rar"),
    (".ras", "image/x-cmu-raster"),
    (".rdf", "application/xml"),
    (".rgb", "image/x-rgb"),
    (".roff", "application/x-troff"),
    (".rpm", "application/x-rpm"),
    (".rst", "text/x-rst"),
    (".rtf", "application/rtf"),
    (".rtx", "text/richtext"),
    (".sgm", "text/x-sgml"),
    (".sgml", "text/x-sgml"),
    (".sh", "application/x-sh"),
    (".shar", "application/x-shar"),
    (".snd", "audio/basic"),
    (".so", "application/octet-stream"),
    (".sql", "application/sql"),
    (".sqlite", "application/vnd.sqlite3"),
    (".sqlite3", "application/vnd.sqlite3"),
    (".src", "application/x-wais-source"),
    (".srt", "text/plain"),
    (".stl", "model/stl"),
    (".sv4cpio", "application/x-sv4cpio"),
    (".sv4crc", "application/x-sv4crc"),
    (".svg", "image/svg+xml"),
    (".swf", "application/x-shockwave-flash"),
    (".t", "application/x-troff"),
    (".t38", "image/t38"),
    (".tar", "application/x-tar"),
    (".tcl", "application/x-tcl"),
    (".tex", "application/x-tex"),
    (".texi", "application/texinfo"),
    (".texinfo", "application/texinfo"),
    (".tfx", "image/tiff-fx"),
    (".thmx", "application/vnd.ms-officetheme"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".toml", "application/toml"),
    (".tr", "application/x-troff"),
    (".trig", "application/trig"),
    (".tsv", "text/tab-separated-values"),
    (".ttc", "font/collection"),
    (".ttf", "font/ttf"),
    (".txt", "text/plain"),
    (".ustar", "application/x-ustar"),
    (".vcf", "text/x-vcard"),
    (".vtt", "text/vtt"),
    (".wasm", "application/wasm"),
    (".wav", "audio/vnd.wave"),
    (".weba", "audio/webm"),
    (".webm", "video/webm"),
    (".webmanifest", "application/manifest+json"),
    (".webp", "image/webp"),
    (".wiz", "application/msword"),
    (".wmf", "image/wmf"),
    (".wmv", "video/x-ms-wmv"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
    (".wsdl", "application/xml"),
    (".xbm", "image/x-xbitmap"),
    (".xlb", "application/vnd.ms-excel"),
    (".xls", "application/vnd.ms-excel"),
    (".xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    (".xml", "text/xml"),
    (".xpdl", "application/xml"),
    (".xpm", "image/x-xpixmap"),
    (".xsl", "application/xml"),
    (".xwd", "image/x-xwindowdump"),
    (".yaml", "application/yaml"),
    (".yml", "application/yaml"),
    (".zip", "application/zip"),
];

const ENCODINGS: &[(&str, &str)] = &[
    (".gz", "gzip"),
    (".Z", "compress"),
    (".bz2", "bzip2"),
    (".xz", "xz"),
    (".br", "br"),
];

const SUFFIXES: &[(&str, &str)] = &[
    (".svgz", ".svg.gz"),
    (".tgz", ".tar.gz"),
    (".taz", ".tar.gz"),
    (".tz", ".tar.gz"),
    (".tbz2", ".tar.bz2"),
    (".txz", ".tar.xz"),
];

/// Moonraker's mimetypes.add_type calls (after mimetypes' own init)
const MOONRAKER_TYPES: [(&str, &str); 3] = [(".log", "text/plain"), (".gcode", "text/plain"), (".cfg", "text/plain")];

thread_local! {
    static MEMO: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
}

fn table(t: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    t.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// posixpath.splitext
fn splitext(path: &str) -> (&str, &str) {
    let sep = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let dot = match path[sep..].rfind('.') {
        Some(i) => sep + i,
        None => return (path, ""),
    };
    // a name of only leading dots has no extension
    if path[sep..dot].chars().all(|c| c == '.') {
        return (path, "");
    }
    (&path[..dot], &path[dot..])
}

/// types_map[True][ext] after init and Moonraker's add_type
fn type_for(ext: &str) -> Option<String> {
    if let Some(t) = table(&MOONRAKER_TYPES, ext) {
        return Some(t.to_string());
    }
    if let Some(v) = MEMO.with(|m| m.borrow().get(ext).cloned()) {
        return v;
    }
    let mut found = DEFAULT_TYPES.binary_search_by(|(k, _)| (*k).cmp(ext)).ok().map(|i| DEFAULT_TYPES[i].1.to_string());
    for file in KNOWN_FILES {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        // MimeTypes.readfp: words up to a comment; the first is the type
        for line in text.lines() {
            let mut words = line.split_whitespace().take_while(|w| !w.starts_with('#'));
            let Some(mime) = words.next() else {
                continue;
            };
            if words.any(|s| s.len() + 1 == ext.len() && ext[1..] == *s) {
                found = Some(mime.to_string());
            }
        }
    }
    MEMO.with(|m| m.borrow_mut().insert(ext.to_string(), found.clone()));
    found
}

/// mimetypes.guess_type(path): (type, encoding)
pub fn guess_type(path: &str) -> (Option<String>, Option<&'static str>) {
    let (b, e) = splitext(path);
    let (mut base, mut ext) = (b.to_string(), e.to_string());
    // suffix_map (.tgz -> .tar.gz), looked up lowercased
    while let Some(rep) = table(SUFFIXES, &ext.to_lowercase()) {
        let joined = format!("{}{}", base, rep);
        let (b, e) = splitext(&joined);
        (base, ext) = (b.to_string(), e.to_string());
    }
    // encodings_map is case sensitive
    let encoding = table(ENCODINGS, &ext);
    if encoding.is_some() {
        let (_, e) = splitext(&base);
        ext = e.to_string();
    }
    (type_for(&ext.to_lowercase()), encoding)
}

/// tornado StaticFileHandler.get_content_type
pub fn content_type(path: &str) -> String {
    match guess_type(path) {
        (_, Some("gzip")) => "application/gzip".into(),
        (_, Some(_)) => "application/octet-stream".into(),
        (Some(t), None) => t,
        (None, None) => "application/octet-stream".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_mimetypes_cases() {
        assert_eq!(content_type("/x/y.gcode"), "text/plain");
        assert_eq!(content_type("a.LOG"), "text/plain");
        assert_eq!(guess_type("a.tar.gz"), (Some("application/x-tar".into()), Some("gzip")));
        assert_eq!(guess_type("a.tgz"), (Some("application/x-tar".into()), Some("gzip")));
        assert_eq!(content_type("a.tgz"), "application/gzip");
        assert_eq!(content_type("a.bz2"), "application/octet-stream");
        assert_eq!(content_type(".bashrc"), "application/octet-stream");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert_eq!(splitext("/a.b/c"), ("/a.b/c", ""));
        assert_eq!(splitext("x/..hidden"), ("x/..hidden", ""));
    }
}
