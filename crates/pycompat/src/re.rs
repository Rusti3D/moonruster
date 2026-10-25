// Compiled regular expressions, cached per pattern as Python's re module
// caches them (re.compile / re.match ... compile a pattern once)
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static CACHE: RefCell<HashMap<String, Option<regex::Regex>>> = RefCell::new(HashMap::new());
}

/// The compiled pattern, None if it does not compile
pub fn try_compile(pattern: &str) -> Option<regex::Regex> {
    CACHE.with(|c| {
        if let Some(r) = c.borrow().get(pattern) {
            return r.clone();
        }
        let r = regex::Regex::new(pattern).ok();
        c.borrow_mut().insert(pattern.to_string(), r.clone());
        r
    })
}

/// The compiled pattern of a pattern known to be valid
pub fn compile(pattern: &str) -> regex::Regex {
    try_compile(pattern).expect("invalid regex")
}
