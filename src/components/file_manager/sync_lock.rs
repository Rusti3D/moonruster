// NotifySyncLock (file_manager.py): a file operation's request waits for its inotify notifications

use super::*;

#[derive(Default)]
pub(super) struct SyncState {
    pub(super) action: String,
    pub(super) dest_path: Option<PathBuf>,
    pub(super) check_pending: bool,
    pub(super) move_copy_evt: Option<Rc<AsyncEvent>>,
    pub(super) move_copy_interrupted: bool,
    pub(super) sync_waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    pub(super) pending_paths: IndexSet<PathBuf>,
    pub(super) acquired_paths: IndexSet<PathBuf>,
}

pub struct NotifySyncLock {
    pub(super) mutex: tokio::sync::Mutex<()>,
    pub(super) locked: Cell<bool>,
    pub(super) state: RefCell<SyncState>,
}

pub struct SyncGuard<'a> {
    pub(super) _guard: tokio::sync::MutexGuard<'a, ()>,
    pub(super) lock: &'a NotifySyncLock,
}

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.set(false);
        self.lock.finish();
    }
}

impl NotifySyncLock {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(NotifySyncLock { mutex: tokio::sync::Mutex::new(()), locked: Cell::new(false), state: RefCell::new(SyncState::default()) })
    }

    pub async fn acquire(&self) -> SyncGuard<'_> {
        let g = self.mutex.lock().await;
        self.locked.set(true);
        SyncGuard { _guard: g, lock: self }
    }

    pub fn setup(&self, action: &str, path: &Path, move_copy: bool) -> Result<(), ServerError> {
        if !self.locked.get() {
            return Err(ServerError::bad("Cannot call setup unless the lock has been acquired"));
        }
        if self.state.borrow().dest_path.is_some() {
            log::debug!("NotifySync Error: Setup requested while a path is still pending");
            self.finish();
        }
        let mut st = self.state.borrow_mut();
        st.dest_path = Some(path.to_path_buf());
        st.action = action.to_string();
        st.check_pending = move_copy;
        Ok(())
    }

    pub async fn wait_inotify_event(&self, current_path: &Path) {
        if !self.state.borrow().check_pending {
            return;
        }
        self.state.borrow_mut().dest_path = Some(current_path.to_path_buf());
        if self.state.borrow().acquired_paths.contains(current_path) {
            return;
        }
        let evt = AsyncEvent::new();
        {
            let mut st = self.state.borrow_mut();
            st.move_copy_evt = Some(evt.clone());
            st.move_copy_interrupted = false;
        }
        let mut has_pending = self.state.borrow().pending_paths.contains(current_path);
        let mut timeout = if has_pending { 1200.0 } else { 1.0 };
        let mut ok = false;
        for _ in 0..5 {
            match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), evt.wait()).await {
                Ok(_) => {
                    // wait_for(shield(fut)) resumes a couple of loop iterations
                    // after the future is set, letting the observer queue its
                    // other notifications before the request finishes
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                    ok = true;
                    break;
                }
                Err(_) => {
                    if timeout > 2.0 {
                        ok = true;
                        break;
                    }
                    has_pending = self.state.borrow().pending_paths.contains(current_path);
                    timeout = if has_pending { 1200.0 } else { 1.0 };
                }
            }
        }
        if !ok {
            log::info!("Failed to receive an inotify event, dest path: {}", current_path.display());
        }
        self.state.borrow_mut().move_copy_evt = None;
    }

    pub fn finish(&self) {
        let mut st = self.state.borrow_mut();
        for w in st.sync_waiters.drain(..) {
            let _ = w.send(());
        }
        st.dest_path = None;
        st.action.clear();
        st.pending_paths.clear();
        st.acquired_paths.clear();
        if let Some(e) = st.move_copy_evt.take() {
            st.move_copy_interrupted = true;
            e.set();
        }
        st.check_pending = false;
    }

    pub fn add_pending_path(&self, action: &str, pending: &Path) {
        let mut st = self.state.borrow_mut();
        let dest = match &st.dest_path {
            Some(d) if st.check_pending && action == st.action => d.clone(),
            _ => return,
        };
        if dest == pending || Some(dest.as_path()) == pending.parent() {
            st.pending_paths.insert(pending.to_path_buf());
        }
    }

    pub fn check_in_request(&self, action: &str, path: &Path) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let mut st = self.state.borrow_mut();
        let dest = st.dest_path.clone()?;
        let mut need_waiter = false;
        if st.check_pending {
            if let Some(evt) = &st.move_copy_evt {
                if dest == path {
                    evt.set();
                    need_waiter = true;
                }
            } else if dest == path || Some(dest.as_path()) == path.parent() {
                st.acquired_paths.insert(path.to_path_buf());
                need_waiter = true;
            }
        } else if dest == path {
            need_waiter = true;
        }
        if !need_waiter {
            return None;
        }
        if action != st.action {
            log::info!(
                "\nInotify action mismatch:\nExpected action: {}, Inotify action: {}\nRequested path: {}\nInotify path: {}\nIs move/copy: {}",
                st.action,
                action,
                dest.display(),
                path.display(),
                crate::pyfmt::py_bool(st.check_pending)
            );
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        st.sync_waiters.push(tx);
        Some(rx)
    }
}
