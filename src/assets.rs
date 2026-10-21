// Embedded assets (moonraker/assets)

pub fn read_asset(name: &str) -> Option<String> {
    match name {
        "welcome.html" => Some(include_str!("../assets/welcome.html").to_string()),
        "default_allowed_services" => Some(include_str!("../assets/default_allowed_services").to_string()),
        _ => None,
    }
}
