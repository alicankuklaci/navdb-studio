//! Background worker owning a driver connection.
//!
//! The GUI never blocks on the network: every database call is sent to a
//! session's worker thread as a closure and the GUI polls the returned
//! [`Pending`] handle each frame.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Result};

use crate::config::ConnectionConfig;
use crate::driver::{self, Driver};

type Job = Box<dyn FnOnce(&mut Option<Box<dyn Driver>>, &ConnectionConfig) + Send>;

pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Handle to a worker thread. Cloning shares the same underlying connection.
#[derive(Clone)]
pub struct Session {
    tx: Sender<Job>,
    cfg: Arc<ConnectionConfig>,
    notify: Notify,
    busy: Arc<Mutex<usize>>,
}

/// Result slot filled by a worker.
pub struct Pending<T> {
    slot: Arc<Mutex<Option<Result<T>>>>,
    done: bool,
}

impl<T> Pending<T> {
    /// Take the result once it is available.
    pub fn poll(&mut self) -> Option<Result<T>> {
        if self.done {
            return None;
        }
        let v = self.slot.lock().ok()?.take();
        if v.is_some() {
            self.done = true;
        }
        v
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Block until ready (used by tests and automation jobs).
    pub fn wait(mut self) -> Result<T> {
        loop {
            if let Some(v) = self.poll() {
                return v;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub fn ready(v: Result<T>) -> Self {
        Self { slot: Arc::new(Mutex::new(Some(v))), done: false }
    }
}

impl Session {
    pub fn spawn(cfg: ConnectionConfig, notify: Notify) -> Session {
        let (tx, rx) = channel::<Job>();
        let cfg = Arc::new(cfg);
        let worker_cfg = cfg.clone();
        thread::Builder::new()
            .name(format!("db-{}", cfg.display_name()))
            .spawn(move || {
                let mut drv: Option<Box<dyn Driver>> = None;
                while let Ok(job) = rx.recv() {
                    job(&mut drv, &worker_cfg);
                }
            })
            .expect("spawning session thread");
        Session { tx, cfg, notify, busy: Arc::new(Mutex::new(0)) }
    }

    pub fn config(&self) -> &ConnectionConfig {
        &self.cfg
    }

    pub fn is_busy(&self) -> bool {
        self.busy.lock().map(|b| *b > 0).unwrap_or(false)
    }

    /// Run `f` with the (lazily connected) driver on the worker thread.
    pub fn run<T, F>(&self, f: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn Driver) -> Result<T> + Send + 'static,
    {
        let slot: Arc<Mutex<Option<Result<T>>>> = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let notify = self.notify.clone();
        let busy = self.busy.clone();
        if let Ok(mut b) = busy.lock() {
            *b += 1;
        }
        let job: Job = Box::new(move |drv, cfg| {
            let res = (|| {
                if drv.is_none() {
                    *drv = Some(driver::connect(cfg)?);
                }
                let d = drv.as_mut().ok_or_else(|| anyhow!("not connected"))?;
                f(d.as_mut())
            })();
            // drop broken connections so the next job reconnects
            if let Err(e) = &res {
                let msg = format!("{:#}", e).to_ascii_lowercase();
                if msg.contains("broken pipe")
                    || msg.contains("connection reset")
                    || msg.contains("server has gone away")
                    || msg.contains("connection closed")
                    || msg.contains("connecting to")
                {
                    *drv = None;
                }
            }
            if let Ok(mut s) = out.lock() {
                *s = Some(res);
            }
            if let Ok(mut b) = busy.lock() {
                *b = b.saturating_sub(1);
            }
            notify();
        });
        if self.tx.send(job).is_err() {
            if let Ok(mut b) = self.busy.lock() {
                *b = b.saturating_sub(1);
            }
            return Pending::ready(Err(anyhow!("session closed")));
        }
        Pending { slot, done: false }
    }

    /// Spawn an independent session with the same configuration (each query
    /// tab gets its own connection, like the reference client).
    pub fn fork(&self) -> Session {
        Session::spawn((*self.cfg).clone(), self.notify.clone())
    }

    /// Drop the current connection; the next job reconnects.
    pub fn reconnect(&self) -> Pending<()> {
        let slot: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let notify = self.notify.clone();
        let _ = self.tx.send(Box::new(move |drv, _| {
            *drv = None;
            if let Ok(mut s) = out.lock() {
                *s = Some(Ok(()));
            }
            notify();
        }));
        Pending { slot, done: false }
    }
}

/// Run `f` on a fresh thread (work that needs no shared session, e.g. Test Connection).
pub fn spawn_task<T, F>(notify: Notify, f: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let slot: Arc<Mutex<Option<Result<T>>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    thread::spawn(move || {
        let r = f();
        if let Ok(mut s) = out.lock() {
            *s = Some(r);
        }
        notify();
    });
    Pending { slot, done: false }
}

pub fn noop_notify() -> Notify {
    Arc::new(|| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DbKind;

    #[test]
    fn session_runs_jobs_in_order() {
        let cfg = ConnectionConfig { database: ":memory:".into(), ..ConnectionConfig::new(DbKind::Sqlite) };
        let s = Session::spawn(cfg, noop_notify());
        let a = s.run(|d| d.execute(None, "CREATE TABLE t(x); INSERT INTO t VALUES (5)"));
        let b = s.run(|d| d.query(None, "SELECT x FROM t"));
        a.wait().unwrap();
        let r = b.wait().unwrap();
        assert_eq!(r.rows[0][0], crate::value::Value::Int(5));
    }

    #[test]
    fn errors_are_reported() {
        let cfg = ConnectionConfig { database: ":memory:".into(), ..ConnectionConfig::new(DbKind::Sqlite) };
        let s = Session::spawn(cfg, noop_notify());
        assert!(s.run(|d| d.query(None, "SELECT * FROM nope")).wait().is_err());
    }
}
