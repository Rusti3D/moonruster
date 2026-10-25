// The uhubctl device type

use super::*;

impl PowerDevice {
    pub(super) async fn uhub_refresh(&self, is_init: bool) {
        match self.run_uhubctl("info").await {
            Err(e) => {
                self.set_state_val("error");
                if !self.should_log_error() {
                    return;
                }
                let ename = if is_init { "Refresh" } else { "Initialization" };
                log::info!("Power Device {}: {} Error\n{}", self.name, ename, e);
            }
            Ok(r) => {
                if !POLLING.with(|p| p.get()) {
                    log::debug!("Power Device {}: uhubctl device info: {}", self.name, crate::pyfmt::py_repr(&r));
                }
                self.set_state_val(r["state"].as_str().unwrap_or("error"));
            }
        }
    }

    /// Returns the parsed port info, the error string includes uhubctl output
    pub(super) async fn run_uhubctl(&self, action: &str) -> Result<Value, String> {
        let (location, port) = match &self.kind {
            Kind::UHub { location, port } => (location.clone(), *port),
            _ => return Err("not uhubctl".into()),
        };
        let mut cmd = format!("uhubctl -l {}", location);
        if let Some(p) = port {
            cmd.push_str(&format!(" -p {}", p));
        }
        let mut search_prefix = format!("Current status for hub {}", location);
        if action == "on" || action == "off" {
            cmd.push_str(&format!(" -a {}", action));
            search_prefix = format!("New status for hub {}", location);
        }
        let shell: Rc<crate::components::shell_command::ShellCommandFactory> =
            server().lookup_component("shell_command").ok_or("shell_command")?;
        let resp = shell
            .exec_cmd(&cmd, crate::components::shell_command::ExecOpts { log_complete: false, ..Default::default() })
            .await
            .map_err(|e| format!("{}\nuhubctrl output: {}", e.message, String::from_utf8_lossy(&e.stderr)))?;
        let re = regex::Regex::new(
            r"(?xi)^\s*
            (?:Port\s(?P<port>[0-9]+):)
            (?:\s(?P<bits>[0-9a-f]{4}))
            (?:\s(?P<pstate>power|off))
            (?P<flags>(?:\s[0-9a-z.]+)+)?
            (?:\s\[(?P<desc>.+)\])?
            \s*$",
        )
        .unwrap();
        for line in resp.lines() {
            if !search_prefix.is_empty() {
                if line.starts_with(&search_prefix) {
                    search_prefix.clear();
                }
                continue;
            }
            let c = match re.captures(line.trim()) {
                None => continue,
                Some(c) => c,
            };
            let pnum: i64 = match c["port"].parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let bits = match i64::from_str_radix(&c["bits"], 16) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if port.map(|p| p != pnum).unwrap_or(false) {
                continue;
            }
            let state = if &c["pstate"] == "power" { "on" } else { "off" };
            let flags: Vec<String> = c.name("flags").map(|f| f.as_str().split_whitespace().map(String::from).collect()).unwrap_or_default();
            return Ok(json!({
                "port": pnum,
                "status_bits": bits,
                "state": state,
                "flags": flags,
                "desc": c.name("desc").map(|d| d.as_str()),
            }));
        }
        Err(format!(
            "Failed to receive response for device at location {}, port {}, ",
            location,
            port.map(|p| p.to_string()).unwrap_or("None".into())
        ))
    }
}
