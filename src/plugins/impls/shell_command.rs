// moonraker.components.shell_command for Python components: ShellCommand and
// the factory's build_shell_command / exec_cmd / run_cmd_async are
// Moonraker's own code (mirror_verbatim.txt: asyncio subprocesses, its
// logging). The factory's running commands are kept here, so Moonruster's
// shutdown cancels them as Moonraker's ShellCommandFactory.close() does.
use super::*;
use crate::plugins::futures::await_py;
use std::cell::RefCell;

macro_rules! factory {
    ($m:literal) => {
        concat!("moonraker.components.shell_command.ShellCommandFactory.", $m)
    };
}

thread_local! {
    static RUNNING: RefCell<Vec<PyRef>> = const { RefCell::new(Vec::new()) };
}

/// ShellCommandFactory.close() for the components' commands
pub async fn cancel_running() {
    let cmds: Vec<PyRef> = RUNNING.with(|r| r.borrow().clone());
    for cmd in cmds {
        let r = match cmd.call_method("cancel", &[]) {
            Ok(coro) => await_py(coro).await,
            Err(e) => Err(e),
        };
        if let Err(e) = r {
            log::error!("moonruster: cancelling a component's shell command\n{}", e.traceback);
        }
    }
}

pub fn register(r: &mut Registry) {
    r.attr(factory!("eventloop"), |_| super::server::event_loop(), None);
    // running_commands: the commands as a set (Moonraker's attribute)
    r.attr(
        factory!("running_commands"),
        |_| {
            let set = lookup("builtins", "set")?.call(&[])?;
            for c in RUNNING.with(|r| r.borrow().clone()) {
                set.call_method("add", &[&c])?;
            }
            Ok(set)
        },
        None,
    );
    r.method(factory!("add_running_command"), |a| {
        let cmd = a.get(0, "cmd").ok_or_else(|| py_error("TypeError", "missing argument 'cmd'".into()))?.clone();
        // a set: added once
        RUNNING.with(|r| {
            let mut r = r.borrow_mut();
            if !r.iter().any(|c| c.as_ptr() == cmd.as_ptr()) {
                r.push(cmd);
            }
        });
        Ok(none())
    });
    r.method(factory!("remove_running_command"), |a| {
        let cmd = a.get(0, "cmd").ok_or_else(|| py_error("TypeError", "missing argument 'cmd'".into()))?;
        RUNNING.with(|r| r.borrow_mut().retain(|c| c.as_ptr() != cmd.as_ptr()));
        Ok(none())
    });
}
