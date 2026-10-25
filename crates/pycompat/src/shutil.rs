// Python's shutil (move, copy2, copytree) and str(OSError), as Moonraker's
// file manager uses them
use crate::posixpath::join;
use std::path::Path;

fn path_join(a: &str, b: &str) -> String {
    join(a, &[b])
}

/// python's str(OSError)
pub fn py_os_error(e: &std::io::Error, path: &str) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let msg = std::io::Error::from_raw_os_error(code).to_string();
            let msg = msg.split(" (os error").next().unwrap_or(&msg).to_string();
            format!("[Errno {}] {}: '{}'", code, msg, path)
        }
        None => e.to_string(),
    }
}


/// shutil.move(src, dst), returns the final destination
pub fn shutil_move(src: &str, dst: &str) -> Result<String, String> {
    let mut real_dst = dst.to_string();
    if Path::new(dst).is_dir() {
        let base = Path::new(src.trim_end_matches('/')).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        real_dst = path_join(dst, &base);
        if Path::new(&real_dst).exists() {
            return Err(format!("Destination path '{}' already exists", real_dst));
        }
    }
    match std::fs::rename(src, &real_dst) {
        Ok(_) => Ok(real_dst),
        Err(_) => {
            if Path::new(src).is_dir() {
                shutil_copytree(src, &real_dst)?;
                std::fs::remove_dir_all(src).map_err(|e| e.to_string())?;
            } else {
                copy_with_meta(src, &real_dst)?;
                std::fs::remove_file(src).map_err(|e| e.to_string())?;
            }
            Ok(real_dst)
        }
    }
}

fn copy_with_meta(src: &str, dst: &str) -> Result<(), String> {
    // copyfile + copystat.  The times are set on the open file before it is
    // closed, so the single inotify CLOSE_WRITE already sees the copied mtime
    // (python sets them by path right after closing, which usually wins the
    // race with its observer)
    let meta = std::fs::metadata(src).map_err(|e| py_os_error(&e, src))?;
    let mut fin = std::fs::File::open(src).map_err(|e| py_os_error(&e, src))?;
    let mut fout = std::fs::File::create(dst).map_err(|e| py_os_error(&e, dst))?;
    std::io::copy(&mut fin, &mut fout).map_err(|e| py_os_error(&e, dst))?;
    let _ = fout.set_permissions(meta.permissions());
    if let (Ok(a), Ok(m)) = (meta.accessed(), meta.modified()) {
        let _ = fout.set_times(std::fs::FileTimes::new().set_accessed(a).set_modified(m));
    }
    Ok(())
}

/// shutil.copy2(src, dst)
pub fn shutil_copy2(src: &str, dst: &str) -> Result<String, String> {
    let mut real = dst.to_string();
    if Path::new(dst).is_dir() {
        real = path_join(dst, &Path::new(src).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
    }
    copy_with_meta(src, &real)?;
    Ok(real)
}

/// shutil.copytree(src, dst)
pub fn shutil_copytree(src: &str, dst: &str) -> Result<String, String> {
    if Path::new(dst).exists() {
        return Err(format!("[Errno 17] File exists: '{}'", dst));
    }
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let s = path_join(src, &name);
        let d = path_join(dst, &name);
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&s).map_err(|e| e.to_string())?;
            if Path::new(&s).is_dir() {
                shutil_copytree(&s, &d)?;
            } else {
                let _ = target;
                copy_with_meta(&s, &d)?;
            }
        } else if ft.is_dir() {
            shutil_copytree(&s, &d)?;
        } else {
            copy_with_meta(&s, &d)?;
        }
    }
    Ok(dst.to_string())
}
