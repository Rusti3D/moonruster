// LDAP authentication (port of moonraker/components/ldap.py)

use crate::common::ServerError;
use crate::confighelper::ConfigHelper;
use crate::server::Component;
use serde_json::json;
use std::rc::Rc;

const VALID_MEMBERSHIP_ATTRS: [&str; 2] = ["memberOf", "isMemberOf"];

pub struct MoonrakerLdap {
    host: String,
    port: Option<i64>,
    secure: bool,
    membership_attr: String,
    check_dn_case: bool,
    base_dn: String,
    group_dn: Option<String>,
    active_directory: bool,
    bind_dn: Option<String>,
    bind_password: Option<String>,
    user_filter: Option<String>,
    lock: tokio::sync::Mutex<()>,
}

impl Component for MoonrakerLdap {}

/// ldap3.utils.conv.escape_filter_chars
fn escape_filter_chars(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\5c"),
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\0' => out.push_str("\\00"),
            c => out.push(c),
        }
    }
    out
}

impl MoonrakerLdap {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let host = config.get("ldap_host")?;
        let port = config.getint_opt("ldap_port", crate::confighelper::Bounds::none())?;
        let secure = config.getboolean_or("ldap_secure", false)?;
        let membership_attr = config.getchoice("membership_attribute", &VALID_MEMBERSHIP_ATTRS, Some("memberOf"), false)?;
        let check_dn_case = config.getboolean_or("check_dn_case", true)?;
        let render = |t: crate::components::template::JinjaTemplate| t.render(&json!({})).map_err(|e| ConfigHelper::error(e.message));
        let base_dn = render(config.gettemplate("base_dn", false)?)?;
        let group_dn = match config.gettemplate_opt("group_dn", Some(None), false)? {
            Some(t) => Some(render(t)?),
            None => None,
        };
        let active_directory = config.getboolean_or("is_active_directory", false)?;
        let bind_dn_t = config.gettemplate_opt("bind_dn", Some(None), false)?;
        let bind_pass_t = config.gettemplate_opt("bind_password", Some(None), false)?;
        let mut bind_dn = None;
        let mut bind_password = None;
        if let Some(t) = bind_dn_t {
            bind_dn = Some(render(t)?);
            match bind_pass_t {
                None => return Err(ConfigHelper::error("Section [ldap]: Option 'bind_password' is required when 'bind_dn' is provided")),
                Some(p) => bind_password = Some(render(p)?),
            }
        }
        let user_filter = match config.gettemplate_opt("user_filter", Some(None), false)? {
            Some(t) => {
                let f = render(t)?;
                if !f.contains("USERNAME") {
                    return Err(ConfigHelper::error("Section [ldap]: Option 'user_filter' is is missing required token USERNAME"));
                }
                Some(f)
            }
            None => None,
        };
        Ok(Rc::new(MoonrakerLdap {
            host,
            port,
            secure,
            membership_attr,
            check_dn_case,
            base_dn,
            group_dn,
            active_directory,
            bind_dn,
            bind_password,
            user_filter,
            lock: tokio::sync::Mutex::new(()),
        }))
    }

    fn url(&self) -> String {
        let scheme = if self.secure { "ldaps" } else { "ldap" };
        let port = self.port.unwrap_or(if self.secure { 636 } else { 389 });
        format!("{}://{}:{}", scheme, self.host, port)
    }

    pub async fn authenticate_ldap_user(&self, username: &str, password: &str) -> Result<(), ServerError> {
        let _g = self.lock.lock().await;
        let unauthorized = || ServerError::new("Unauthorized", 401);
        let res: Result<bool, ldap3::LdapError> = async {
            let settings = ldap3::LdapConnSettings::new().set_conn_timeout(std::time::Duration::from_secs(10));
            let (conn, mut ldap) = ldap3::LdapConnAsync::with_settings(settings, &self.url()).await?;
            ldap3::drive!(conn);
            ldap.simple_bind(self.bind_dn.as_deref().unwrap_or(""), self.bind_password.as_deref().unwrap_or(""))
                .await?
                .success()?;
            let attr_name = if self.active_directory { "sAMAccountName" } else { "uid" };
            let escaped = escape_filter_chars(username);
            let filter = match &self.user_filter {
                Some(f) => f.replace("USERNAME", &escaped),
                None => format!("(&(objectClass=Person)({}={}))", attr_name, escaped),
            };
            let attrs: Vec<&str> = match &self.group_dn {
                None => vec!["*"],
                Some(_) => vec![self.membership_attr.as_str()],
            };
            let (entries, _) = ldap.search(&self.base_dn, ldap3::Scope::Subtree, &filter, attrs).await?.success()?;
            let entry = match entries.into_iter().next() {
                Some(e) => ldap3::SearchEntry::construct(e),
                None => {
                    log::info!("LDAP User '{}' Not Found", username);
                    return Ok(false);
                }
            };
            let rebind = ldap.simple_bind(&entry.dn, password).await?.success();
            let _ = ldap.unbind().await;
            if rebind.is_err() {
                log::debug!("LDAP Rebind failed, attempting to validate credentials with new connection.");
                let (conn2, mut ldap2) = ldap3::LdapConnAsync::with_settings(
                    ldap3::LdapConnSettings::new().set_conn_timeout(std::time::Duration::from_secs(10)),
                    &self.url(),
                )
                .await?;
                ldap3::drive!(conn2);
                ldap2.simple_bind(&entry.dn, password).await?.success()?;
                let _ = ldap2.unbind().await;
            }
            Ok(self.validate_group(username, &entry))
        }
        .await;
        match res {
            Ok(true) => Ok(()),
            Ok(false) => {
                log::info!("Invalid LDAP Username or Password");
                Err(unauthorized())
            }
            Err(e) => {
                log::error!("LDAP authentication failed\n{}", e);
                Err(unauthorized())
            }
        }
    }

    fn validate_group(&self, username: &str, user: &ldap3::SearchEntry) -> bool {
        let req = match &self.group_dn {
            None => {
                log::debug!("LDAP User {} login successful", username);
                return true;
            }
            Some(g) => g.clone(),
        };
        let groups = match user.attrs.get(&self.membership_attr) {
            None => {
                log::info!("Membership attribute {} not present in user entry", self.membership_attr);
                return false;
            }
            Some(g) => g.clone(),
        };
        log::debug!("Required Group DN: {}, User Group DNs: {:?}", req, groups);
        let (groups, req) = if !self.check_dn_case {
            (groups.iter().map(|g| g.to_lowercase()).collect::<Vec<_>>(), req.to_lowercase())
        } else {
            (groups, req)
        };
        if groups.contains(&req) {
            log::debug!("LDAP User {} group match success, login successful", username);
            return true;
        }
        log::info!("User '{}' is not a member of configured group_dn.", username);
        false
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<MoonrakerLdap>, crate::confighelper::ConfigError> {
    MoonrakerLdap::new(config)
}
