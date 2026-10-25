// The tplink_smartplug type

use super::*;

impl PowerDevice {
    pub(super) fn tplink_encrypt(data: &[u8]) -> Vec<u8> {
        let mut key: u8 = 0xAB;
        let mut res = (data.len() as u32).to_be_bytes().to_vec();
        for c in data {
            let v = key ^ c;
            key = v;
            res.push(v);
        }
        res
    }

    pub(super) fn tplink_decrypt(data: &[u8]) -> String {
        let mut key: u8 = 0xAB;
        let mut res = String::new();
        for c in data {
            res.push((key ^ c) as char);
            key = *c;
        }
        res
    }

    pub(super) fn tplink_command<'a>(&'a self, command: &'a str) -> futures::future::LocalBoxFuture<'a, Result<Value, ServerError>> {
        Box::pin(async move {
            let (timer, addr, output_id, port) = match &self.kind {
                Kind::TpLink { timer, addr, output_id, port } => (timer.clone(), addr.clone(), *output_id, *port),
                _ => return Err(err("not tplink")),
            };
            let out_cmd = match command {
                "on" | "off" => {
                    let mut c = json!({"system": {"set_relay_state": {"state": (command == "on") as i64}}});
                    if let Some(oid) = output_id {
                        let sysinfo = self.tplink_command("info").await?;
                        let child_id = sysinfo["system"]["get_sysinfo"]["children"]
                            .get(oid as usize)
                            .and_then(|c| c.get("id"))
                            .map(crate::pyfmt::py_str)
                            .ok_or_else(|| err("IndexError: list index out of range"))?;
                        c["context"] = json!({"child_ids": [child_id]});
                    }
                    c
                }
                "info" => json!({"system": {"get_sysinfo": {}}}),
                "clear_rules" => json!({"count_down": {"delete_all_rules": null}}),
                "count_off" => {
                    let delay: i64 = timer.trim().parse().map_err(|_| err(format!("invalid literal for int() with base 10: '{}'", timer)))?;
                    json!({"count_down": {"add_rule": {"enable": 1, "delay": delay, "act": 0, "name": "turn off"}}})
                }
                _ => return Err(err(format!("Invalid tplink command: {}", command))),
            };
            let res: Result<Vec<u8>, String> = async {
                let mut stream = tokio::net::TcpStream::connect(format!("{}:{}", addr, port)).await.map_err(|e| e.to_string())?;
                stream.write_all(&Self::tplink_encrypt(out_cmd.to_string().as_bytes())).await.map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 2048];
                let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n < 4 {
                    return Err("unpack requires a buffer of 4 bytes".into());
                }
                let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                let mut data = buf[4..n].to_vec();
                let mut retries = 5;
                let mut remaining = length.saturating_sub(data.len());
                while remaining > 0 && retries > 0 {
                    let mut b = vec![0u8; remaining];
                    let n = stream.read(&mut b).await.map_err(|e| e.to_string())?;
                    data.extend_from_slice(&b[..n]);
                    remaining = length.saturating_sub(data.len());
                    retries -= 1;
                }
                if retries == 0 {
                    return Err("Unable to read tplink packet".into());
                }
                Ok(data)
            }
            .await;
            let data = match res {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Error sending tplink command: {}", command);
                    log::error!("{}\n{}", msg, e);
                    return Err(err(msg));
                }
            };
            serde_json::from_str(&Self::tplink_decrypt(&data)).map_err(|e| ServerError::new(e.to_string(), 500))
        })
    }

    pub(super) async fn tplink_info(&self) -> Result<i64, ServerError> {
        let res = self.tplink_command("info").await?;
        let output_id = match &self.kind {
            Kind::TpLink { output_id, .. } => *output_id,
            _ => None,
        };
        let v = match output_id {
            Some(oid) => res["system"]["get_sysinfo"]["children"].get(oid as usize).and_then(|c| c.get("state")).cloned(),
            None => res["system"]["get_sysinfo"].get("relay_state").cloned(),
        };
        v.and_then(|v| v.as_i64()).ok_or_else(|| err("KeyError"))
    }
}
