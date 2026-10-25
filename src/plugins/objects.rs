// Moonruster objects seen from Python: each Rust object handed to a
// component gets one Python object of its mirrored class (so `is` holds),
// created without running __init__ and carrying a handle (`_mr_h`) back to
// the Rust object. Objects a component constructs from a mirrored class are
// bound to their Rust object by _mr.new.
// From kaliru's src/plugins/objects.rs (@ b0e1435).
//
// The table holds both sides of each pair. A pair nobody else holds (the
// Rust object only by the table, the Python object only by the table) is
// released by a sweep that runs as the table grows: the Python object keeps
// the Rust one alive as Python would (a component may hold a closed
// connection), and Moonruster keeping the Rust object keeps the Python one
// (with what a component set on it), as Moonraker's own references would.
//
// Only the loop thread touches the table (native.rs refuses calls from
// other threads), so it is thread-local.
use super::py::*;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

struct Entry {
    rust: Rc<dyn Any>,
    py: PyRef,
}

#[derive(Default)]
struct Table {
    // handle -> pair; handles are not reused
    entries: HashMap<usize, Entry>,
    next: usize,
    // entry count that triggers the next sweep
    sweep_at: usize,
    // Rust object address -> handle
    by_addr: HashMap<usize, usize>,
    // Rust type -> mirrored class (module, class name)
    classes: HashMap<TypeId, (&'static str, &'static str)>,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

fn addr(rust: &Rc<dyn Any>) -> usize {
    Rc::as_ptr(rust) as *const () as usize
}

fn type_error(msg: String) -> PyError {
    PyError { type_name: "TypeError".into(), message: msg, traceback: String::new(), value: None }
}

/// Register the Moonraker class of a Rust type whose path does not name it
pub fn register_class<T: 'static>(module: &'static str, class: &'static str) {
    TABLE.with(|t| t.borrow_mut().classes.insert(TypeId::of::<T>(), (module, class)));
}

/// The Moonraker class of a Rust type: registered, else by its path
/// (moonruster::components::database::MoonrakerDatabase is
/// moonraker.components.database.MoonrakerDatabase)
fn class_of(tid: TypeId, type_name: &str) -> PyResult<PyRef> {
    if let Some((module, class)) = TABLE.with(|t| t.borrow().classes.get(&tid).copied()) {
        return lookup(module, class);
    }
    let path = type_name.split('<').next().unwrap_or(type_name);
    let mut parts: Vec<&str> = path.split("::").collect();
    let class = parts.pop().unwrap_or("");
    if parts.first() == Some(&"moonruster") && parts.len() > 1 {
        parts[0] = "moonraker";
        if let Ok(c) = lookup(&parts.join("."), class) {
            return Ok(c);
        }
    }
    Err(type_error(format!("moonruster: {} has no Moonraker class yet (plugins/objects.rs register_class)", type_name)))
}

/// The Python object of a Rust object (created on first use)
pub fn wrap<T: 'static>(rust: Rc<T>) -> PyResult<PyRef> {
    wrap_any(rust, std::any::type_name::<T>())
}

/// wrap() of an object known by its Rust type's path only
pub fn wrap_any(rust: Rc<dyn Any>, type_name: &str) -> PyResult<PyRef> {
    if let Some(py) = existing(&rust) {
        return Ok(py);
    }
    let cls = class_of((*rust).type_id(), type_name)?;
    // cls.__new__(cls): the object exists already, __init__ must not run
    let py = cls.getattr("__new__")?.call(&[&cls])?;
    bind(&py, rust)?;
    Ok(py)
}

/// wrap() with the Moonraker class given (built-in components: the class
/// their load_component returns, mirror_ids.rs COMPONENT_CLASSES)
pub fn wrap_class(rust: Rc<dyn Any>, module: &str, class: &str) -> PyResult<PyRef> {
    if let Some(py) = existing(&rust) {
        return Ok(py);
    }
    let cls = lookup(module, class)?;
    let py = cls.getattr("__new__")?.call(&[&cls])?;
    bind(&py, rust)?;
    Ok(py)
}

/// Make `py` the Python object of `rust` (_mr.new)
pub fn bind(py: &PyRef, rust: Rc<dyn Any>) -> PyResult<()> {
    if TABLE.with(|t| {
        let t = t.borrow();
        t.entries.len() >= t.sweep_at
    }) {
        sweep()?;
    }
    let handle = TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let h = t.next;
        t.next += 1;
        t.by_addr.insert(addr(&rust), h);
        t.entries.insert(h, Entry { rust, py: py.clone() });
        h
    });
    // object.__setattr__: Object.__setattr__ would report a patch
    lookup("builtins", "object")?.getattr("__setattr__")?.call(&[py, &py_str("_mr_h"), &py_int(handle as i64)])?;
    Ok(())
}

/// The Rust object behind a Python object of a mirrored class
pub fn unwrap(py: &PyRef) -> PyResult<Rc<dyn Any>> {
    let h = match py.getattr("_mr_h") {
        Ok(h) => h.as_i64()? as usize,
        Err(_) => {
            return Err(type_error("moonruster: the object is not bound to a Moonruster object (its __init__ did not run)".into()))
        }
    };
    TABLE
        .with(|t| t.borrow().entries.get(&h).map(|e| e.rust.clone()))
        .ok_or_else(|| PyError {
            type_name: "ReferenceError".into(),
            message: "moonruster: the object no longer exists".into(),
            traceback: String::new(),
            value: None,
        })
}

/// The Rust object of a given type behind a Python object
pub fn unwrap_as<T: 'static>(py: &PyRef) -> PyResult<Rc<T>> {
    unwrap(py)?
        .downcast::<T>()
        .map_err(|_| type_error(format!("moonruster: expected a {}", std::any::type_name::<T>())))
}

/// The Python object already bound to a Rust object, if any
pub fn existing(rust: &Rc<dyn Any>) -> Option<PyRef> {
    TABLE.with(|t| {
        let t = t.borrow();
        t.by_addr.get(&addr(rust)).and_then(|h| t.entries.get(h).map(|e| e.py.clone()))
    })
}

/// Release the pairs nobody else holds; the next sweep runs when the table
/// has doubled (at least 64 entries), so the cost per bind stays constant
fn sweep() -> PyResult<()> {
    let getrefcount = lookup("sys", "getrefcount")?;
    // the count of an object held by one reference only, as measured here
    // (what getrefcount's own call adds differs between Python versions)
    let base = getrefcount.call(&[&lookup("builtins", "object")?.call(&[])?])?.as_i64()?;
    let handles: Vec<(usize, PyRef)> =
        TABLE.with(|t| t.borrow().entries.iter().filter(|(_, e)| Rc::strong_count(&e.rust) == 1).map(|(h, e)| (*h, e.py.clone())).collect());
    let mut free = Vec::new();
    for (h, py) in handles {
        // the table's reference and `py` here
        if getrefcount.call(&[&py])?.as_i64()? == base + 1 {
            free.push(h);
        }
    }
    // drop outside the borrow: a Python object's finalizer may call back in
    let released: Vec<Entry> = TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let out: Vec<Entry> = free.iter().filter_map(|h| t.entries.remove(h)).collect();
        for e in &out {
            t.by_addr.remove(&addr(&e.rust));
        }
        t.sweep_at = (t.entries.len() * 2).max(64);
        out
    });
    for h in &free {
        super::native::forget_instance(*h as i64);
    }
    drop(released);
    Ok(())
}
