// moonraker.components.template.TemplateFactory / JinjaTemplate for Python
// components (ConfigHelper.gettemplate, Moonraker's own code, creates them)
use super::*;
use crate::components::template::{JinjaTemplate, TemplateFactory};
use crate::plugins::futures::rust_future;

fn context(a: &Args) -> PyResult<Value> {
    match a.get(0, "context") {
        Some(c) if !c.is_none() => from_py(c),
        _ => Ok(Value::Object(Default::default())),
    }
}

pub fn register(r: &mut Registry) {
    // add_environment_global(name, value): a value, or a callable the
    // templates call (Python's own function, on the event loop)
    r.method("moonraker.components.template.TemplateFactory.add_environment_global", |a| {
        let name = a.get(0, "name").ok_or_else(|| py_error("TypeError", "missing argument 'name'".into()))?.as_string()?;
        let value = a.get(1, "value").ok_or_else(|| py_error("TypeError", "missing argument 'value'".into()))?.clone();
        let global = if value.is_callable() {
            TemplateFactory::global_function(Rc::new(move |args: Vec<Value>, kwargs: serde_json::Map<String, Value>| {
                let r: PyResult<Value> = (|| {
                    let pargs: Vec<PyRef> = args.iter().map(to_py).collect::<PyResult<_>>()?;
                    let kw = to_py(&Value::Object(kwargs))?;
                    from_py(&value.call_kw(&pargs.iter().collect::<Vec<_>>(), &kw)?)
                })();
                r.map_err(|e| crate::common::ServerError::new(e.message, 500))
            }))
        } else {
            crate::components::template::TemplateGlobal::Data(from_py(&value)?)
        };
        a.this::<TemplateFactory>()?.add_environment_global(&name, global).map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
    r.method("moonraker.components.template.TemplateFactory.create_ui_template", |a| {
        let source = a.get(0, "source").ok_or_else(|| py_error("TypeError", "missing argument 'source'".into()))?.as_string()?;
        let t = a.this::<TemplateFactory>()?.create_ui_template(&source).map_err(|e| server_error(&e.message, e.status_code))?;
        objects::wrap_class(Rc::new(t), "moonraker.components.template", "JinjaTemplate")
    });
    r.method("moonraker.components.template.TemplateFactory.create_template", |a| {
        let source = a.get(0, "source").ok_or_else(|| py_error("TypeError", "missing argument 'source'".into()))?.as_string()?;
        let is_async = match a.get(1, "is_async") {
            Some(v) => v.is_true()?,
            None => false,
        };
        let t = a.this::<TemplateFactory>()?.compile(&source, is_async).map_err(|e| {
            // Jinja2 raises TemplateSyntaxError (the engine's message: the
            // template engine is minijinja); other errors as ServerError
            if e.kind() == minijinja::ErrorKind::SyntaxError {
                let detail = e.detail().unwrap_or("syntax error").to_string();
                let line = py_int(e.line().unwrap_or(1) as i64);
                match lookup("jinja2", "TemplateSyntaxError").and_then(|c| c.call(&[&py_str(&detail), &line])) {
                    Ok(exc) => PyError { type_name: "TemplateSyntaxError".into(), message: detail, traceback: String::new(), value: Some(exc) },
                    Err(err) => err,
                }
            } else {
                server_error(&e.to_string(), 500)
            }
        })?;
        objects::wrap_class(Rc::new(t), "moonraker.components.template", "JinjaTemplate")
    });
    r.method("moonraker.components.template.JinjaTemplate.render", |a| {
        let ctx = context(a)?;
        a.this::<JinjaTemplate>()?.render(&ctx).map(|s| py_str(&s)).map_err(|e| server_error(&e.message, e.status_code))
    });
    r.method("moonraker.components.template.JinjaTemplate.render_async", |a| {
        let ctx = context(a)?;
        let t = a.this::<JinjaTemplate>()?;
        rust_future(async move { t.render_async(&ctx).await.map(|s| py_str(&s)).map_err(|e| server_error(&e.message, e.status_code)) })
    });
}
