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

fn base_env(secrets: &Value, custom: bool) -> Environment<'static> {
    let mut env = Environment::new();
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
        if let Err(e) = self.jenv.template_from_str(source) {
            log::error!("Error creating template from source:\n{}\n{}", source, e);
            return Err(ServerError::new(format!("{}", e), 500));
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
