// Webcam snapshots

use super::*;

impl SimplyPrint {
    pub(super) async fn initialize_url(&self) {
        let wcm: Option<Rc<crate::components::webcam::WebcamManager>> = server().lookup_component("webcam");
        let cams = match wcm {
            Some(w) => w.get_webcams(),
            None => return,
        };
        if cams.is_empty() {
            return;
        }
        let cam = match cams.get(&self.webcam_name).filter(|_| !self.webcam_name.is_empty()) {
            Some(c) => c.clone(),
            None => cams.values().next().unwrap().clone(),
        };
        match cam.get_snapshot_url(true).await {
            Err(e) => log::error!("Failed to retrieve webcam url\n{}", e.message),
            Ok(url) => {
                log::info!("SimplyPrint Webcam URL assigned: {}", url);
                *self.webcam_url.borrow_mut() = url;
                *self.webcam_cam.borrow_mut() = Some(cam);
            }
        }
    }

    pub(super) fn http() -> Rc<HttpClient> {
        server().lookup_component("http_client").expect("http_client")
    }

    pub(super) async fn test_connection(&self) {
        let url = self.webcam_url.borrow().clone();
        if !url.starts_with("http") {
            self.webcam_connected.set(false);
            return;
        }
        let resp = Self::http().get(&url, &[("Accept".into(), "image/jpeg".into())], false).await;
        self.webcam_connected.set(!resp.has_error());
    }

    pub(super) async fn extract_image(&self) -> Result<String, ServerError> {
        use base64::Engine;
        let url = self.webcam_url.borrow().clone();
        let resp = Self::http().get(&url, &[("Accept".into(), "image/jpeg".into())], false).await;
        resp.raise_for_status(None)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&resp.content))
    }

    pub(super) async fn post_image(&self, payload: Map<String, Value>) {
        let res: Result<(), ServerError> = async {
            if let Some(uid) = payload.get("id").filter(|v| !v.is_null()) {
                let url = payload.get("endpoint").map(crate::pyfmt::py_str).unwrap_or(SP_SNAPSHOT_URL.into());
                let img = self.extract_image().await?;
                let headers = vec![
                    ("User-Agent".to_string(), "Mozilla/5.0".to_string()),
                    ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ];
                let esc = |s: &str| crate::utils::url_escape(s).replace("%20", "+");
                let body = format!("id={}&image={}", esc(&crate::pyfmt::py_str(uid)), esc(&img));
                let resp = Self::http().post(&url, Body::Text(body), &headers, RequestOpts::default()).await;
                resp.raise_for_status(None)?;
            } else if let Some(timer) = payload.get("timer").and_then(|v| v.as_f64()) {
                tokio::time::sleep(Duration::from_secs_f64((timer / 1000.0).max(0.0))).await;
                let img = self.extract_image().await?;
                self.send_sp("stream", json!({"base": img}));
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            if server().is_verbose_enabled() {
                log::error!("SimplyPrint WebCam Stream Error\n{}", e.message);
            }
        }
    }
}
