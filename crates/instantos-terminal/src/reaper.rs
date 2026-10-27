//! Child ownership for detached launches. One sleeping worker per caller process,
//! independent of how many terminal windows it opens.
use std::io;
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

pub(super) struct Reaper {
    children: Mutex<Vec<Child>>,
    changed: Condvar,
}

pub(super) fn shared() -> io::Result<&'static Arc<Reaper>> {
    static REAPER: OnceLock<io::Result<Arc<Reaper>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let reaper = Arc::new(Reaper {
                children: Mutex::new(Vec::new()),
                changed: Condvar::new(),
            });
            let worker = Arc::clone(&reaper);
            std::thread::Builder::new()
                .name("terminal-child-reaper".into())
                .spawn(move || worker.run())?;
            Ok(reaper)
        })
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

impl Reaper {
    pub(super) fn watch(&self, child: Child) {
        self.children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(child);
        self.changed.notify_one();
    }

    fn run(&self) {
        let mut children = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            while children.is_empty() {
                children = self
                    .changed
                    .wait(children)
                    .unwrap_or_else(|error| error.into_inner());
            }
            children.retain_mut(|child| match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) => false,
                Err(error) => error.kind() == io::ErrorKind::Interrupted,
            });
            if !children.is_empty() {
                (children, _) = self
                    .changed
                    .wait_timeout(children, Duration::from_millis(250))
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
    }
}
