//! One bounded backend queue shared by accounts, legacy writes and downloads.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub(crate) const EXECUTION_SLOTS: usize = 4;
pub(crate) const ADMISSION_REQUESTS: usize = 20;
pub(crate) const ACCOUNT_BASE_SLOTS: usize = 2;
pub(crate) const BACKGROUND_SLOTS: usize = 1;
pub(crate) const QUEUE_WAIT: Duration = Duration::from_secs(10);
type Account = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkClass {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionError {
    Capacity,
    QueueTimeout,
    AccountCancelled,
}

struct Waiter {
    id: u64,
    account: Account,
    class: WorkClass,
    granted: bool,
    wake: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct State {
    closed: bool,
    admitted: usize,
    running: usize,
    background: usize,
    accounts: HashMap<Account, usize>,
    next_id: u64,
    queue: VecDeque<Waiter>,
}

impl State {
    fn account_running(&self, account: &Account) -> usize {
        self.accounts.get(account).copied().unwrap_or(0)
    }

    fn class_available(&self, class: WorkClass) -> bool {
        class == WorkClass::Foreground || self.background < BACKGROUND_SLOTS
    }

    fn next_waiter(&self) -> Option<usize> {
        // Foreground owns first choice of a free slot. Within its class,
        // restore accounts' basic shares before making new loans.
        for class in [WorkClass::Foreground, WorkClass::Background] {
            let runnable = |waiter: &Waiter| {
                !waiter.granted && waiter.class == class && self.class_available(class)
            };
            if let Some(index) = self
                .queue
                .iter()
                .position(|waiter| {
                    runnable(waiter) && self.account_running(&waiter.account) < ACCOUNT_BASE_SLOTS
                })
                .or_else(|| self.queue.iter().position(runnable))
            {
                return Some(index);
            }
        }
        None
    }

    fn release_slots(&mut self, account: Account, class: WorkClass) {
        self.running = self.running.saturating_sub(1);
        if class == WorkClass::Background {
            self.background = self.background.saturating_sub(1);
        }
        if let Some(running) = self.accounts.get_mut(&account) {
            *running = running.saturating_sub(1);
            if *running == 0 {
                self.accounts.remove(&account);
            }
        }
    }

    fn dispatch(&mut self) {
        while self.running < EXECUTION_SLOTS {
            let Some(index) = self.next_waiter() else {
                break;
            };
            let waiter = &mut self.queue[index];
            let account = waiter.account;
            let class = waiter.class;
            waiter.granted = true;
            let wake = waiter.wake.take();
            self.running += 1;
            *self.accounts.entry(account).or_default() += 1;
            if class == WorkClass::Background {
                self.background += 1;
            }
            let delivered = wake.is_some_and(|wake| wake.send(()).is_ok());
            if !delivered {
                // A dropped receiving future cannot reserve a slot. Its RAII
                // ticket may still be unwinding; removing by id is idempotent.
                self.queue.remove(index);
                self.admitted = self.admitted.saturating_sub(1);
                self.release_slots(account, class);
            }
        }
    }
}

pub(crate) struct Scheduler {
    state: Mutex<State>,
    drained: Notify,
    queue_wait: Duration,
}

impl Scheduler {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            drained: Notify::new(),
            queue_wait: QUEUE_WAIT,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called after admitted writes have drained. Close shared admission and
    /// wait for existing read permits, including detached blocking work.
    pub(super) async fn stop_and_drain(&self, deadline: Instant) -> bool {
        self.state().closed = true;
        loop {
            if self.state().admitted == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, self.drained.notified())
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    fn dispatch(&self, state: &mut State) {
        state.dispatch();
        if state.closed && state.admitted == 0 {
            self.drained.notify_one();
        }
    }

    pub(crate) async fn acquire(
        self: &Arc<Self>,
        account: Account,
        class: WorkClass,
        cancellation: &CancellationToken,
    ) -> Result<BackendPermit, AdmissionError> {
        if cancellation.is_cancelled() {
            return Err(AdmissionError::AccountCancelled);
        }
        let (wake, receiver) = oneshot::channel();
        let id = {
            let mut state = self.state();
            let capacity = ADMISSION_REQUESTS - usize::from(class == WorkClass::Background);
            if state.closed || state.admitted >= capacity {
                return Err(AdmissionError::Capacity);
            }
            let id = state.next_id;
            state.next_id = id.checked_add(1).ok_or(AdmissionError::Capacity)?;
            state.admitted += 1;
            state.queue.push_back(Waiter {
                id,
                account,
                class,
                granted: false,
                wake: Some(wake),
            });
            id
        };
        let mut ticket = QueuedTicket {
            scheduler: self.clone(),
            id,
            active: true,
        };
        self.dispatch(&mut self.state());
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AdmissionError::AccountCancelled),
            result = tokio::time::timeout(self.queue_wait, receiver) => {
                match result {
                    Err(_) => return Err(AdmissionError::QueueTimeout),
                    Ok(Err(_)) => return Err(AdmissionError::Capacity),
                    Ok(Ok(())) => {}
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(AdmissionError::AccountCancelled);
        }
        let mut state = self.state();
        let index = state
            .queue
            .iter()
            .position(|waiter| waiter.id == id && waiter.granted)
            .ok_or(AdmissionError::Capacity)?;
        state.queue.remove(index);
        ticket.active = false;
        Ok(BackendPermit {
            scheduler: self.clone(),
            account,
            class,
        })
    }
}

struct QueuedTicket {
    scheduler: Arc<Scheduler>,
    id: u64,
    active: bool,
}
impl Drop for QueuedTicket {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.scheduler.state();
        if let Some(index) = state.queue.iter().position(|waiter| waiter.id == self.id)
            && let Some(waiter) = state.queue.remove(index)
        {
            state.admitted = state.admitted.saturating_sub(1);
            if waiter.granted {
                state.release_slots(waiter.account, waiter.class);
            }
            self.scheduler.dispatch(&mut state);
        }
    }
}

pub(crate) struct BackendPermit {
    scheduler: Arc<Scheduler>,
    account: Account,
    class: WorkClass,
}
impl Drop for BackendPermit {
    fn drop(&mut self) {
        let mut state = self.scheduler.state();
        state.admitted = state.admitted.saturating_sub(1);
        state.release_slots(self.account, self.class);
        self.scheduler.dispatch(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    fn poll<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }
    fn counts(scheduler: &Scheduler) -> (usize, usize, usize, usize) {
        let state = scheduler.state();
        (
            state.admitted,
            state.running,
            state.background,
            state.queue.len(),
        )
    }
    async fn full(
        scheduler: &Arc<Scheduler>,
        cancellation: &CancellationToken,
    ) -> Vec<BackendPermit> {
        let mut permits = Vec::new();
        for _ in 0..EXECUTION_SLOTS {
            permits.push(
                scheduler
                    .acquire([1; 32], WorkClass::Foreground, cancellation)
                    .await
                    .unwrap(),
            );
        }
        permits
    }

    #[tokio::test]
    async fn loans_are_not_preempted_and_new_accounts_receive_their_basic_share_first() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let mut permits = full(&scheduler, &cancellation).await;
        let mut loan = Box::pin(scheduler.acquire([1; 32], WorkClass::Foreground, &cancellation));
        let mut other1 = Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancellation));
        let mut other2 = Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancellation));
        assert!(poll(loan.as_mut()).is_pending());
        assert!(poll(other1.as_mut()).is_pending());
        assert!(poll(other2.as_mut()).is_pending());
        assert_eq!(scheduler.state().account_running(&[1; 32]), 4);
        permits.pop();
        let first = other1.await.unwrap();
        assert!(poll(loan.as_mut()).is_pending());
        permits.pop();
        let second = other2.await.unwrap();
        assert!(poll(loan.as_mut()).is_pending());
        assert_eq!(scheduler.state().account_running(&[2; 32]), 2);
        permits.pop();
        let loan = loan.await.unwrap();
        drop((permits, first, second, loan));
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }
}
