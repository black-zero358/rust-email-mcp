//! One bounded backend queue shared by accounts, legacy writes and downloads.
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::oneshot;
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
        // First restore accounts below their basic share. Within the same
        // priority tier foreground work wins, then FIFO order is preserved.
        for class in [WorkClass::Foreground, WorkClass::Background] {
            if let Some(index) = self.queue.iter().position(|waiter| {
                !waiter.granted
                    && waiter.class == class
                    && self.class_available(class)
                    && self.account_running(&waiter.account) < ACCOUNT_BASE_SLOTS
            }) {
                return Some(index);
            }
        }
        // Existing loans are not preempted. New loans stop while another
        // runnable account below its basic share has work waiting. Background
        // waiters blocked by the global background cap cannot use an idle slot.
        for class in [WorkClass::Foreground, WorkClass::Background] {
            if let Some(index) = self.queue.iter().position(|candidate| {
                !candidate.granted
                    && candidate.class == class
                    && self.class_available(class)
                    && !self.queue.iter().any(|waiter| {
                        !waiter.granted
                            && waiter.account != candidate.account
                            && self.class_available(waiter.class)
                            && self.account_running(&waiter.account) < ACCOUNT_BASE_SLOTS
                    })
            }) {
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
            if wake.is_none_or(|wake| wake.send(()).is_err()) {
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
    queue_wait: Duration,
}

impl Scheduler {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            queue_wait: QUEUE_WAIT,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            if state.admitted >= ADMISSION_REQUESTS {
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
        self.state().dispatch();
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

    /// Cancel-safe reads/background work. Writes instead hold acquire()'s permit
    /// until their durable send outcome is known; revocation must not cut SMTP.
    pub(crate) async fn run<T, F>(
        self: &Arc<Self>,
        account: Account,
        class: WorkClass,
        cancellation: &CancellationToken,
        operation: impl FnOnce() -> F,
    ) -> Result<T, AdmissionError>
    where
        F: Future<Output = T>,
    {
        let _permit = self.acquire(account, class, cancellation).await?;
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AdmissionError::AccountCancelled),
            result = operation() => Ok(result),
        }
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
            state.dispatch();
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
        state.dispatch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
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
    async fn admission_bound_counts_running_waiting_and_unstarted_drops() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let unstarted = scheduler.acquire([1; 32], WorkClass::Foreground, &cancellation);
        drop(unstarted);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
        let permits = full(&scheduler, &cancellation).await;
        let mut waiting = Vec::new();
        for _ in 0..ADMISSION_REQUESTS - EXECUTION_SLOTS {
            let mut future =
                Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancellation));
            assert!(poll(future.as_mut()).is_pending());
            waiting.push(future);
        }
        assert_eq!(counts(&scheduler), (20, 4, 0, 16));
        assert!(matches!(
            scheduler
                .acquire([3; 32], WorkClass::Foreground, &cancellation)
                .await,
            Err(AdmissionError::Capacity)
        ));
        drop(waiting);
        assert_eq!(counts(&scheduler), (4, 4, 0, 0));
        drop(permits);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
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

    #[tokio::test]
    async fn foreground_precedes_queued_background_and_background_uses_at_most_one_slot() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let mut permits = full(&scheduler, &cancellation).await;
        let mut bg1 = Box::pin(scheduler.acquire([2; 32], WorkClass::Background, &cancellation));
        let mut bg2 = Box::pin(scheduler.acquire([3; 32], WorkClass::Background, &cancellation));
        let mut foreground =
            Box::pin(scheduler.acquire([4; 32], WorkClass::Foreground, &cancellation));
        assert!(poll(bg1.as_mut()).is_pending());
        assert!(poll(bg2.as_mut()).is_pending());
        assert!(poll(foreground.as_mut()).is_pending());
        permits.pop();
        let foreground = foreground.await.unwrap();
        assert!(poll(bg1.as_mut()).is_pending());
        permits.pop();
        let first = bg1.await.unwrap();
        assert_eq!(scheduler.state().background, 1);
        permits.pop();
        assert!(poll(bg2.as_mut()).is_pending());
        drop(first);
        let second = bg2.await.unwrap();
        assert_eq!(scheduler.state().background, 1);
        drop((permits, foreground, second));
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn dropping_a_granted_but_unclaimed_future_releases_its_reserved_slot() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let mut permits = full(&scheduler, &cancellation).await;
        let mut future = Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancellation));
        assert!(poll(future.as_mut()).is_pending());
        permits.pop();
        assert_eq!(counts(&scheduler), (4, 4, 0, 1));
        drop(future);
        assert_eq!(counts(&scheduler), (3, 3, 0, 0));
        drop(permits);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn a_background_waiter_blocked_by_its_class_cap_does_not_prevent_foreground_loans() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let background = scheduler
            .acquire([1; 32], WorkClass::Background, &cancellation)
            .await
            .unwrap();
        let first = scheduler
            .acquire([2; 32], WorkClass::Foreground, &cancellation)
            .await
            .unwrap();
        let second = scheduler
            .acquire([2; 32], WorkClass::Foreground, &cancellation)
            .await
            .unwrap();
        let mut waiting_background =
            Box::pin(scheduler.acquire([3; 32], WorkClass::Background, &cancellation));
        assert!(poll(waiting_background.as_mut()).is_pending());
        let mut loan = Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancellation));
        let loan = match poll(loan.as_mut()) {
            Poll::Ready(Ok(permit)) => permit,
            _ => panic!("foreground can borrow the idle slot"),
        };
        assert_eq!(counts(&scheduler), (5, 4, 1, 1));
        drop((loan, first, second, background));
        let second_background = waiting_background.await.unwrap();
        assert_eq!(scheduler.state().background, 1);
        drop(second_background);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn background_basic_share_precedes_another_accounts_foreground_loan() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let mut permits = full(&scheduler, &cancellation).await;
        let mut loan = Box::pin(scheduler.acquire([1; 32], WorkClass::Foreground, &cancellation));
        let mut background =
            Box::pin(scheduler.acquire([2; 32], WorkClass::Background, &cancellation));
        assert!(poll(loan.as_mut()).is_pending());
        assert!(poll(background.as_mut()).is_pending());
        permits.pop();
        let background = background.await.unwrap();
        assert!(poll(loan.as_mut()).is_pending());
        assert_eq!(scheduler.state().background, 1);
        drop(background);
        let loan = loan.await.unwrap();
        drop((permits, loan));
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn cancellation_wins_over_a_ready_grant_and_does_not_leak_admission() {
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let mut permits = full(&scheduler, &cancellation).await;
        let cancelled = CancellationToken::new();
        let mut future = Box::pin(scheduler.acquire([2; 32], WorkClass::Foreground, &cancelled));
        assert!(poll(future.as_mut()).is_pending());
        permits.pop();
        cancelled.cancel();
        assert!(matches!(
            future.await,
            Err(AdmissionError::AccountCancelled)
        ));
        assert_eq!(counts(&scheduler), (3, 3, 0, 0));
        drop(permits);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
        assert!(matches!(
            scheduler
                .acquire([2; 32], WorkClass::Foreground, &cancelled)
                .await,
            Err(AdmissionError::AccountCancelled)
        ));
    }

    #[tokio::test]
    async fn queue_timeout_cleans_waiter_and_preserves_active_permits() {
        let scheduler = Arc::new(Scheduler {
            state: Mutex::new(State::default()),
            queue_wait: Duration::from_millis(5),
        });
        let cancellation = CancellationToken::new();
        let permits = full(&scheduler, &cancellation).await;
        assert_eq!(
            scheduler
                .acquire([2; 32], WorkClass::Foreground, &cancellation)
                .await
                .err(),
            Some(AdmissionError::QueueTimeout)
        );
        assert_eq!(counts(&scheduler), (4, 4, 0, 0));
        drop(permits);
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }

    #[tokio::test]
    async fn cancelled_queued_operations_never_start_and_running_task_abort_releases_capacity() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let scheduler = Scheduler::new();
        let cancellation = CancellationToken::new();
        let permits = full(&scheduler, &cancellation).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        let mut future =
            Box::pin(
                scheduler.run([2; 32], WorkClass::Foreground, &token, || async {
                    starts.fetch_add(1, Ordering::SeqCst);
                }),
            );
        assert!(poll(future.as_mut()).is_pending());
        token.cancel();
        assert_eq!(future.await, Err(AdmissionError::AccountCancelled));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        drop(permits);
        let running = scheduler.clone();
        let started = starts.clone();
        let task = tokio::spawn(async move {
            let token = CancellationToken::new();
            running
                .run([3; 32], WorkClass::Foreground, &token, || async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await
                })
                .await
        });
        while starts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
        drop(
            scheduler
                .acquire([3; 32], WorkClass::Foreground, &cancellation)
                .await,
        );
        assert_eq!(counts(&scheduler), (0, 0, 0, 0));
    }
}
