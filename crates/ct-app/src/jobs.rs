//! Async contract (PLAN §9.2): fixed worker pool (4 threads), `JobId{kind, generation}`,
//! stale-result discard, cooperative cancellation, bounded (1024) result channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const WORKERS: usize = 4;
pub const RESULT_CAP: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum JobKind {
    Open,
    Refs,
    Log,
    Diff,
    FileDiff,
    Search,
    Files,
    Blame,
    History,
    Records,
    Ask,
    Load,
    Save,
    Watch,
    Prompt,
    Accounts,
    Runtime,
    Models,
    Login,
    Logout,
    RunsOpen,
    RunSubmit,
    RunControl,
    RunFinish,
    RunReview,
    Browse,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct JobId {
    pub kind: JobKind,
    pub generation: u64,
}

/// Handed to the job closure.
pub struct JobCtx<R> {
    pub id: JobId,
    pub cancel: Arc<AtomicBool>,
    tx: SyncSender<(JobId, R)>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl<R> JobCtx<R> {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    /// Streaming send. Returns false when the job should stop: cancelled, UI gone, or the
    /// bounded channel is full (searches abort rather than buffer without limit).
    pub fn stream(&self, r: R) -> bool {
        if self.cancelled() {
            return false;
        }
        match self.tx.try_send((self.id, r)) {
            Ok(()) => {
                (self.repaint)();
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }
    /// Final result delivered even when this job raised its own cancel flag (e.g. after hitting
    /// a cap). Staleness is still decided by generation.
    pub fn finish_forced(&self, mut r: R) {
        loop {
            match self.tx.try_send((self.id, r)) {
                Ok(()) => {
                    (self.repaint)();
                    return;
                }
                Err(TrySendError::Full((_, back))) => {
                    r = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
    /// Final result: wait for room (unless cancelled).
    pub fn finish(&self, mut r: R) {
        loop {
            if self.cancelled() {
                return;
            }
            match self.tx.try_send((self.id, r)) {
                Ok(()) => {
                    (self.repaint)();
                    return;
                }
                Err(TrySendError::Full((_, back))) => {
                    r = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

type Task = Box<dyn FnOnce() + Send>;

pub struct Jobs<R: Send + 'static> {
    task_tx: Option<std::sync::mpsc::Sender<Task>>,
    res_tx: SyncSender<(JobId, R)>,
    res_rx: Receiver<(JobId, R)>,
    current: HashMap<JobKind, (u64, Arc<AtomicBool>)>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    next_gen: u64,
}

impl<R: Send + 'static> Jobs<R> {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_workers(WORKERS, repaint)
    }

    pub fn with_workers(n: usize, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (task_tx, task_rx) = std::sync::mpsc::channel::<Task>();
        let task_rx = Arc::new(Mutex::new(task_rx));
        for i in 0..n {
            let rx = task_rx.clone();
            let _ = std::thread::Builder::new().name(format!("ct-worker-{i}")).spawn(move || loop {
                let task = { rx.lock().unwrap().recv() };
                match task {
                    Ok(t) => t(),
                    Err(_) => break,
                }
            });
        }
        let (res_tx, res_rx) = sync_channel(RESULT_CAP);
        Jobs { task_tx: Some(task_tx), res_tx, res_rx, current: HashMap::new(), repaint: Arc::new(repaint), next_gen: 0 }
    }

    /// Start a job. The previous job of the same kind is cancelled and its results will be dropped.
    pub fn spawn(&mut self, kind: JobKind, f: impl FnOnce(&JobCtx<R>) + Send + 'static) -> JobId {
        if let Some((_, old)) = self.current.get(&kind) {
            old.store(true, Ordering::Relaxed);
        }
        self.next_gen += 1;
        let id = JobId { kind, generation: self.next_gen };
        let cancel = Arc::new(AtomicBool::new(false));
        self.current.insert(kind, (id.generation, cancel.clone()));
        let ctx = JobCtx { id, cancel, tx: self.res_tx.clone(), repaint: self.repaint.clone() };
        if let Some(tx) = &self.task_tx {
            let _ = tx.send(Box::new(move || f(&ctx)));
        }
        id
    }

    /// Cancel the running job of `kind` (its late results are discarded too).
    pub fn cancel(&mut self, kind: JobKind) {
        if let Some((_, c)) = self.current.remove(&kind) {
            c.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_current(&self, id: JobId) -> bool {
        self.current.get(&id.kind).map_or(false, |(g, _)| *g == id.generation)
    }

    /// Drain pending results, dropping anything from a superseded generation.
    pub fn poll(&mut self) -> Vec<(JobId, R)> {
        let mut out = Vec::new();
        while let Ok((id, r)) = self.res_rx.try_recv() {
            if self.is_current(id) {
                out.push((id, r));
            }
        }
        out
    }
}

impl<R: Send + 'static> Drop for Jobs<R> {
    fn drop(&mut self) {
        for (_, c) in self.current.values() {
            c.store(true, Ordering::Relaxed);
        }
        self.task_tx.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for<R: Send + 'static>(j: &mut Jobs<R>, want: usize) -> Vec<(JobId, R)> {
        let t = Instant::now();
        let mut all = Vec::new();
        while all.len() < want && t.elapsed() < Duration::from_secs(5) {
            all.extend(j.poll());
            std::thread::sleep(Duration::from_millis(2));
        }
        all
    }

    #[test]
    fn stale_generation_results_are_discarded() {
        let mut j: Jobs<&'static str> = Jobs::new(|| {});
        let a = j.spawn(JobKind::Diff, |c| {
            // loops until superseded, then still tries to deliver a (stale) result
            while !c.cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            c.tx.send((c.id, "stale")).unwrap();
        });
        let b = j.spawn(JobKind::Diff, |c| c.finish("fresh"));
        assert_ne!(a.generation, b.generation);
        let got = wait_for(&mut j, 1);
        std::thread::sleep(Duration::from_millis(30));
        let late = j.poll();
        assert_eq!(got.iter().map(|x| x.1).collect::<Vec<_>>(), vec!["fresh"]);
        assert!(late.is_empty(), "stale result leaked: {:?}", late.iter().map(|x| x.1).collect::<Vec<_>>());
    }

    #[test]
    fn kinds_are_independent() {
        let mut j: Jobs<u8> = Jobs::new(|| {});
        j.spawn(JobKind::Log, |c| c.finish(1));
        j.spawn(JobKind::Search, |c| c.finish(2));
        let mut v: Vec<u8> = wait_for(&mut j, 2).into_iter().map(|x| x.1).collect();
        v.sort();
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn spawning_same_kind_sets_cancel_flag_of_previous() {
        let mut j: Jobs<()> = Jobs::new(|| {});
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        j.spawn(JobKind::Search, move |c| {
            while !c.cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            f2.store(true, Ordering::SeqCst);
        });
        j.spawn(JobKind::Search, |_| {});
        let t = Instant::now();
        while !flag.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(flag.load(Ordering::SeqCst), "old job never saw cancel");
    }

    #[test]
    fn explicit_cancel_drops_late_results() {
        let mut j: Jobs<u8> = Jobs::new(|| {});
        let id = j.spawn(JobKind::Ask, |c| {
            std::thread::sleep(Duration::from_millis(20));
            let _ = c.tx.send((c.id, 9));
        });
        j.cancel(JobKind::Ask);
        assert!(!j.is_current(id));
        std::thread::sleep(Duration::from_millis(80));
        assert!(j.poll().is_empty());
    }

    #[test]
    fn full_channel_makes_streaming_stop() {
        let mut j: Jobs<u32> = Jobs::new(|| {});
        let sent = Arc::new(Mutex::new(0usize));
        let s2 = sent.clone();
        j.spawn(JobKind::Search, move |c| {
            let mut n = 0;
            while c.stream(n) {
                n += 1;
                *s2.lock().unwrap() = n as usize;
            }
        });
        let t = Instant::now();
        loop {
            let n = *sent.lock().unwrap();
            if n >= RESULT_CAP || t.elapsed() > Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(*sent.lock().unwrap(), RESULT_CAP, "producer must stop at the bound");
    }
}
