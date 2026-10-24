// Template Factory helper (port of moonraker/components/template.py)
//
// Jinja2 is replaced by minijinja.  Config templates use '{' '}' as variable
// delimiters like Moonraker.

use crate::common::ServerError;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use minijinja::syntax::SyntaxConfig;
use minijinja::{Environment, Error, ErrorKind};
use serde_json::Value;
use std::rc::Rc;

pub struct TemplateFactory {
    jenv: Rc<Environment<'static>>,
    ui_env: Rc<Environment<'static>>,
}

impl Component for TemplateFactory {}

fn fromjson(s: String) -> Result<minijinja::Value, Error> {
    let v: Value = serde_json::from_str(&s).map_err(|e| Error::new(ErrorKind::InvalidOperation, e.to_string()))?;
    Ok(minijinja::Value::from_serialize(&v))
}

fn raise_error(msg: String, code: Option<i64>) -> Result<String, Error> {
    let _ = code;
    Err(Error::new(ErrorKind::InvalidOperation, msg))
}

fn log_debug(msg: minijinja::Value) -> String {
    log::debug!("{}", msg);
    String::new()
}

/// Output of `{{ value }}` as Jinja2 writes it: str(value). Strings and
/// undefined (empty) as minijinja does; plain data with Python's str()
/// (pyfmt); anything else (functions, objects) with minijinja's default.
fn py_formatter(out: &mut minijinja::Output, state: &minijinja::State, value: &minijinja::Value) -> Result<(), Error> {
    use minijinja::value::ValueKind;
    match value.kind() {
        ValueKind::Undefined | ValueKind::String => minijinja::escape_formatter(out, state, value),
        ValueKind::None | ValueKind::Bool | ValueKind::Number | ValueKind::Seq | ValueKind::Map => {
            match serde_json::to_value(value) {
                Ok(v) => write!(out, "{}", crate::pyfmt::py_str(&v)).map_err(Error::from),
                Err(_) => minijinja::escape_formatter(out, state, value),
            }
        }
        _ => minijinja::escape_formatter(out, state, value),
    }
}

fn base_env(secrets: &Value, custom: bool) -> Environment<'static> {
    let mut env = Environment::new();
    // Jinja2 runs on Python objects: their methods (str.replace, dict.items...)
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    // ...and prints them with str(): True, None, ['a', 'b'], {'k': 1}
    env.set_formatter(py_formatter);
    if custom {
        let syntax = SyntaxConfig::builder()
            .block_delimiters("{%", "%}")
            .variable_delimiters("{", "}")
            .comment_delimiters("{#", "#}")
            .build()
            .expect("valid syntax");
        env.set_syntax(syntax);
    }
    env.set_keep_trailing_newline(false);
    env.add_filter("fromjson", fromjson);
    env.add_function("raise_error", raise_error);
    env.add_function("log_debug", log_debug);
    env.add_global("secrets", minijinja::Value::from_serialize(secrets));
    env
}

impl TemplateFactory {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        srv.load_component(config, "secrets")?;
        let secrets: Rc<crate::components::secrets::Secrets> = srv.lookup_component("secrets").expect("secrets");
        let sv = secrets.values();
        Ok(Rc::new(TemplateFactory { jenv: Rc::new(base_env(&sv, true)), ui_env: Rc::new(base_env(&sv, false)) }))
    }

    pub fn create_template(&self, source: &str, is_async: bool) -> Result<JinjaTemplate, ServerError> {
        self.compile(source, is_async).map_err(|e| ServerError::new(format!("{}", e), 500))
    }

    /// create_template with the template engine's error (its kind and line:
    /// Python components get Jinja2's exception type for a syntax error)
    pub fn compile(&self, source: &str, is_async: bool) -> Result<JinjaTemplate, minijinja::Error> {
        if let Err(e) = self.jenv.template_from_str(source) {
            log::error!("Error creating template from source:\n{}\n{}", source, e);
            return Err(e);
        }
        Ok(JinjaTemplate { source: source.to_string(), env: self.jenv.clone(), is_async })
    }

    pub fn create_ui_template(&self, source: &str) -> Result<JinjaTemplate, ServerError> {
        if let Err(e) = self.ui_env.template_from_str(source) {
            log::error!("Error creating template from source:\n{}\n{}", source, e);
            return Err(ServerError::new(format!("{}", e), 500));
        }
        Ok(JinjaTemplate { source: source.to_string(), env: self.ui_env.clone(), is_async: true })
    }
}

pub type AsyncFn = Rc<dyn Fn(Vec<Value>, serde_json::Map<String, Value>) -> futures::future::LocalBoxFuture<'static, Result<Value, ServerError>>>;

/// Builds a template object on the render thread, the object may call
/// back into the event loop through the Caller
pub type ObjBuilder = Box<dyn FnOnce(Caller) -> minijinja::Value + Send>;

pub struct Call {
    name: String,
    args: Vec<Value>,
    kwargs: serde_json::Map<String, Value>,
    reply: std::sync::mpsc::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct Caller {
    tx: tokio::sync::mpsc::UnboundedSender<Call>,
}

impl Caller {
    /// Blocking call of an async function on the event loop
    pub fn call(&self, name: &str, args: Vec<Value>, kwargs: serde_json::Map<String, Value>) -> Result<minijinja::Value, Error> {
        let (reply, rx) = std::sync::mpsc::channel();
        self.tx
            .send(Call { name: name.to_string(), args, kwargs, reply })
            .map_err(|_| Error::new(ErrorKind::InvalidOperation, "event loop unavailable"))?;
        match rx.recv() {
            Ok(Ok(v)) => Ok(minijinja::Value::from_serialize(&v)),
            Ok(Err(e)) => Err(Error::new(ErrorKind::InvalidOperation, e)),
            Err(_) => Err(Error::new(ErrorKind::InvalidOperation, "call aborted")),
        }
    }
}

/// A mutable dict shared between renders (python dicts passed in a context)
#[derive(Debug, Default)]
pub struct SharedDict(pub std::sync::Mutex<serde_json::Map<String, Value>>);

impl minijinja::value::Object for SharedDict {
    fn repr(self: &std::sync::Arc<Self>) -> minijinja::value::ObjectRepr {
        minijinja::value::ObjectRepr::Map
    }

    fn get_value(self: &std::sync::Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let k = key.as_str()?;
        self.0.lock().unwrap().get(k).map(minijinja::Value::from_serialize)
    }

    fn enumerate(self: &std::sync::Arc<Self>) -> minijinja::value::Enumerator {
        let keys: Vec<minijinja::Value> = self.0.lock().unwrap().keys().map(|k| minijinja::Value::from(k.clone())).collect();
        minijinja::value::Enumerator::Values(keys)
    }

    fn call_method(
        self: &std::sync::Arc<Self>,
        _state: &minijinja::State,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, Error> {
        let to_json = |v: &minijinja::Value| serde_json::to_value(v).unwrap_or(Value::Null);
        let key = || args.first().and_then(|a| a.as_str()).map(String::from).ok_or_else(|| Error::new(ErrorKind::MissingArgument, "key"));
        let mut m = self.0.lock().unwrap();
        match method {
            "update" => {
                if let Some(Value::Object(o)) = args.first().map(to_json) {
                    for (k, v) in o {
                        m.insert(k, v);
                    }
                }
                Ok(minijinja::Value::from(()))
            }
            "get" => Ok(m.get(&key()?).map(minijinja::Value::from_serialize).unwrap_or_else(|| args.get(1).cloned().unwrap_or(minijinja::Value::from(())))),
            "setdefault" => {
                let k = key()?;
                let d = args.get(1).map(to_json).unwrap_or(Value::Null);
                Ok(minijinja::Value::from_serialize(m.entry(k).or_insert(d)))
            }
            "pop" => {
                let k = key()?;
                match m.shift_remove(&k) {
                    Some(v) => Ok(minijinja::Value::from_serialize(&v)),
                    None => args.get(1).cloned().ok_or_else(|| Error::new(ErrorKind::InvalidOperation, format!("KeyError: '{}'", k))),
                }
            }
            "keys" => Ok(minijinja::Value::from_serialize(m.keys().collect::<Vec<_>>())),
            "values" => Ok(minijinja::Value::from_serialize(m.values().collect::<Vec<_>>())),
            "items" => Ok(minijinja::Value::from_serialize(m.iter().collect::<Vec<_>>())),
            "clear" => {
                m.clear();
                Ok(minijinja::Value::from(()))
            }
            _ => Err(Error::new(ErrorKind::UnknownMethod, format!("dict has no method {}", method))),
        }
    }
}

pub fn kwargs_to_map(kwargs: &minijinja::value::Kwargs) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    let keys: Vec<String> = kwargs.args().map(String::from).collect();
    for k in keys {
        if let Ok(v) = kwargs.get::<minijinja::Value>(&k) {
            m.insert(k, serde_json::to_value(&v).unwrap_or(Value::Null));
        }
    }
    m
}

#[derive(Clone)]
pub struct JinjaTemplate {
    source: String,
    env: Rc<Environment<'static>>,
    pub is_async: bool,
}

impl std::fmt::Display for JinjaTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source)
    }
}

impl JinjaTemplate {
    pub fn source(&self) -> &str {
        &self.source
    }

    fn do_render(&self, context: &Value) -> Result<String, Error> {
        let tmpl = self.env.template_from_str(&self.source)?;
        tmpl.render(minijinja::Value::from_serialize(context))
    }

    /// Renders with extra (non serializable) context entries such as callables
    pub fn render_with(&self, context: &Value, extra: Vec<(&str, minijinja::Value)>) -> Result<String, ServerError> {
        let mut ctx: std::collections::BTreeMap<String, minijinja::Value> = std::collections::BTreeMap::new();
        if let Value::Object(m) = context {
            for (k, v) in m {
                ctx.insert(k.clone(), minijinja::Value::from_serialize(v));
            }
        }
        for (k, v) in extra {
            ctx.insert(k.to_string(), v);
        }
        let res = self.env.template_from_str(&self.source).and_then(|t| t.render(minijinja::Value::from(ctx)));
        match res {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) => {
                log::debug!("Template render error: {}", e);
                Err(ServerError::new("Error rending Jinja2 Template", 500))
            }
        }
    }

    pub fn render(&self, context: &Value) -> Result<String, ServerError> {
        if self.is_async {
            return Err(ServerError::bad("Cannot render async templates with the render() method, use render_async()"));
        }
        match self.do_render(context) {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) => {
                log::debug!("Template render error: {}", e);
                Err(ServerError::new("Error rending Jinja2 Template", 500))
            }
        }
    }

    /// Async render where the template may call async functions running on
    /// the server's event loop.  The template runs on a worker thread and
    /// blocks on each call while the event loop services it.
    pub async fn render_async_ext(
        &self,
        context: &Value,
        funcs: Vec<(&str, AsyncFn)>,
        objects: Vec<(&str, ObjBuilder)>,
    ) -> Result<String, ServerError> {
        let env: Environment<'static> = (*self.env).clone();
        let source = self.source.clone();
        let ctx = context.clone();
        let names: Vec<String> = funcs.iter().map(|(n, _)| n.to_string()).collect();
        let fmap: std::collections::HashMap<String, AsyncFn> = funcs.into_iter().map(|(n, f)| (n.to_string(), f)).collect();
        let objects: Vec<(String, ObjBuilder)> = objects.into_iter().map(|(n, o)| (n.to_string(), o)).collect();
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel::<Call>();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        std::thread::spawn(move || {
            let mut env = env;
            let caller = Caller { tx: call_tx };
            for name in names {
                let c = caller.clone();
                let n = name.clone();
                env.add_function(name, move |args: minijinja::value::Rest<minijinja::Value>, kwargs: minijinja::value::Kwargs| {
                    let args: Vec<Value> = args.iter().map(|a| serde_json::to_value(a).unwrap_or(Value::Null)).collect();
                    c.call(&n, args, kwargs_to_map(&kwargs))
                });
            }
            let mut ctx_map: std::collections::BTreeMap<String, minijinja::Value> = std::collections::BTreeMap::new();
            if let Value::Object(m) = &ctx {
                for (k, v) in m {
                    ctx_map.insert(k.clone(), minijinja::Value::from_serialize(v));
                }
            }
            for (name, build) in objects {
                ctx_map.insert(name, build(caller.clone()));
            }
            let res = env
                .template_from_str(&source)
                .and_then(|t| t.render(minijinja::Value::from(ctx_map)))
                .map_err(|e| format!("{:#}", e));
            let _ = done_tx.send(res);
        });
        let res = loop {
            tokio::select! {
                r = &mut done_rx => break r.unwrap_or_else(|_| Err("render thread exited".into())),
                Some(call) = call_rx.recv() => {
                    match fmap.get(&call.name).cloned() {
                        None => { let _ = call.reply.send(Err(format!("unknown function {}", call.name))); }
                        Some(f) => {
                            crate::server::spawn(async move {
                                let r = f(call.args, call.kwargs).await.map_err(|e| e.message);
                                let _ = call.reply.send(r);
                            });
                        }
                    }
                }
            }
        };
        match res {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) => {
                log::debug!("Template render error: {}", e);
                Err(ServerError::new("Error rending Jinja2 Template", 500))
            }
        }
    }

    pub async fn render_async(&self, context: &Value) -> Result<String, ServerError> {
        match self.do_render(context) {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) => {
                log::debug!("Template render error: {}", e);
                Err(ServerError::new("Error rending Jinja2 Template", 500))
            }
        }
    }
}

/// Loads (if needed) and returns the template factory
pub fn get_factory(config: &ConfigHelper) -> Result<Rc<TemplateFactory>, crate::confighelper::ConfigError> {
    let srv = server();
    if let Some(t) = srv.lookup_component::<TemplateFactory>("template") {
        return Ok(t);
    }
    srv.load_component(config, "template")?;
    srv.lookup_component("template").ok_or_else(|| ConfigHelper::error("template not loaded"))
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<TemplateFactory>, crate::confighelper::ConfigError> {
    TemplateFactory::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(env: &Environment, src: &str, ctx: minijinja::Value) -> String {
        env.render_str(src, ctx).unwrap()
    }

    #[test]
    fn python_methods() {
        // Jinja2 templates call methods of Python objects; Moonraker's own
        // welcome.html does: item["entry_id"].replace("/", "-")
        let env = base_env(&serde_json::json!({}), false);
        let ctx = minijinja::context! { item => minijinja::context! { entry_id => "arksine/moonraker/issue/1" }, d => minijinja::context! { a => 1 } };
        assert_eq!(render(&env, r#"{{ "announcement-" + item["entry_id"].replace("/", "-") }}"#, ctx.clone()), "announcement-arksine-moonraker-issue-1");
        assert_eq!(render(&env, r#"{{ "abc".upper() }} {{ "a,b".split(",") }}"#, ctx.clone()), "ABC ['a', 'b']");
        assert_eq!(render(&env, r#"{% for k, v in d.items() %}{{ k }}={{ v }}{% endfor %}"#, ctx), "a=1");
        // values print as Jinja2 prints them (Python str(), checked against
        // jinja2 3.1.6: "['a', 'b']|True|None|{'a': 1}|1.0")
        let vals = minijinja::context! { x => true, n => (), d => minijinja::context! { a => 1 }, f => 1.0 };
        assert_eq!(render(&env, r#"{{ "a,b".split(",") }}|{{ x }}|{{ n }}|{{ d }}|{{ f }}"#, vals), "['a', 'b']|True|None|{'a': 1}|1.0");
        // the config templates' syntax ({ } variables) gets them too
        let cenv = base_env(&serde_json::json!({}), true);
        assert_eq!(render(&cenv, r#"{"on".upper()}"#, minijinja::context! {}), "ON");
    }
}
