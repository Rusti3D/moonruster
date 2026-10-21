// Streaming multipart file upload (FileUploadHandler from moonraker/components/application.py)

use super::FileManager;
use crate::common::UserInfo;
use crate::components::application::{HttpError, HttpRequest};
use futures::StreamExt;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::Request;
use serde_json::{json, Map, Value};
use sha2::Digest;
use std::rc::Rc;
use tokio::io::AsyncWriteExt;

pub async fn process_upload(
    req: Request<Incoming>,
    info: &HttpRequest,
    fm: &Rc<FileManager>,
    max_upload_size: u64,
    user: Option<UserInfo>,
) -> Result<Value, HttpError> {
    let ctype = info.header("Content-Type").unwrap_or_default();
    let tmpname = fm.gen_temp_upload_path();
    let fail = |tmp: &str| {
        let _ = std::fs::remove_file(tmp);
        HttpError::with_reason(500, "File Upload Parsing Failed")
    };
    let boundary = match multer::parse_boundary(&ctype) {
        Ok(b) => b,
        Err(_) => {
            log::error!("Chunk Parsing Error: invalid multipart boundary");
            return Err(fail(&tmpname));
        }
    };
    let body = req.into_body();
    let stream = http_body_util::BodyStream::new(body).filter_map(|frame| async move {
        match frame {
            Ok(f) => f.into_data().ok().map(Ok::<_, std::io::Error>),
            Err(e) => Some(Err(std::io::Error::new(std::io::ErrorKind::Other, e))),
        }
    });
    let constraints = multer::Constraints::new().size_limit(multer::SizeLimit::new().whole_stream(max_upload_size));
    let mut multipart = multer::Multipart::with_constraints(stream, boundary, constraints);
    let mut values: Map<String, Value> = Map::new();
    let mut targets: std::collections::HashMap<&str, Vec<u8>> = std::collections::HashMap::new();
    let mut mp_filename: Option<String> = None;
    let mut hasher = sha2::Sha256::new();
    let mut file_written = false;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                log::error!("Chunk Parsing Error\n{}", e);
                return Err(fail(&tmpname));
            }
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            mp_filename = field.file_name().map(String::from);
            let mut f = match tokio::fs::File::create(&tmpname).await {
                Ok(f) => f,
                Err(_) => return Err(fail(&tmpname)),
            };
            let mut field = field;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        hasher.update(&chunk);
                        if f.write_all(&chunk).await.is_err() {
                            return Err(fail(&tmpname));
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::error!("Chunk Parsing Error\n{}", e);
                        return Err(fail(&tmpname));
                    }
                }
            }
            let _ = f.flush().await;
            file_written = true;
        } else if ["root", "print", "path", "checksum"].contains(&name.as_str()) {
            let key: &str = match name.as_str() {
                "root" => "root",
                "print" => "print",
                "path" => "path",
                _ => "checksum",
            };
            let data = field.bytes().await.map_err(|_| fail(&tmpname))?;
            targets.insert(key, data.to_vec());
        } else {
            // unregistered fields are discarded
            let _ = field.bytes().await;
        }
    }
    let _ = file_written;
    let calc = hex::encode(hasher.finalize()).to_lowercase();
    if let Some(chk) = targets.remove("checksum").filter(|c| !c.is_empty()) {
        let recd = String::from_utf8_lossy(&chk).to_lowercase();
        if calc != recd {
            let _ = std::fs::remove_file(&tmpname);
            return Err(HttpError::with_reason(422, format!("File checksum mismatch: expected {}, calculated {}", recd, calc)));
        }
    }
    let fname = match mp_filename {
        Some(f) if !f.trim().is_empty() => f,
        _ => {
            let _ = std::fs::remove_file(&tmpname);
            return Err(HttpError::with_reason(400, "Multipart filename omitted"));
        }
    };
    for key in ["root", "print", "path"] {
        if let Some(v) = targets.get(key) {
            if !v.is_empty() {
                values.insert(key.into(), json!(String::from_utf8_lossy(v)));
            }
        }
    }
    values.insert("filename".into(), json!(fname));
    values.insert("tmp_file_path".into(), json!(tmpname));
    let mut dbg = String::from("\nFile Upload Arguments:");
    for (k, v) in &values {
        dbg.push_str(&format!("\n{}: {}", k, crate::pyfmt::py_str(v)));
    }
    dbg.push_str(&format!("\nChecksum: {}", calc));
    log::debug!("{}", dbg);
    log::info!("Processing Uploaded File: {}", fname);
    fm.finalize_upload(values, user).await.map_err(|e| HttpError::from_server_error(&e))
}
