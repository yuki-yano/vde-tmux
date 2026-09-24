//! Bounded structural reads and journal acquisition outside the serial mutation path.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::question_notice::journal::{Evaluation, JournalGuard, JournalLocation, writer_state};
use crate::question_notice::resolver::{Binding, Fence, Sample};
use crate::question_notice::resolver::{OrderCheck, RetainReason};
use crate::question_notice::turn_order::{Cursor, REQUEST_BYTES, ReadFailure, SLICE_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerCheck {
    Full,
    CapturedPane,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    WaitBeforeLock,
    NoWaitAfterCapture,
}
pub type VerifyOwner = Arc<dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync>;
pub type MutationBusy = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub struct SharedJournalGuard(Arc<Mutex<Option<JournalGuard>>>);
impl std::fmt::Debug for SharedJournalGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SharedJournalGuard")
    }
}
impl PartialEq for SharedJournalGuard {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for SharedJournalGuard {}
impl SharedJournalGuard {
    pub fn new(guard: JournalGuard) -> Self {
        let deadline = guard.deadline();
        let transfer = Self(Arc::new(Mutex::new(Some(guard))));
        // A single bounded reaper releases guards even when the mutation queue
        // cannot take their completions. The receiver never owns a queued flock.
        type Expiry = (Instant, std::sync::Weak<Mutex<Option<JournalGuard>>>);
        static REAPER: std::sync::OnceLock<mpsc::SyncSender<Expiry>> = std::sync::OnceLock::new();
        let sender = REAPER.get_or_init(|| {
            let (sender, receive) = mpsc::sync_channel::<Expiry>(128);
            std::thread::spawn(move || {
                let mut pending = Vec::<Expiry>::new();
                loop {
                    let next = if let Some(deadline) =
                        pending.iter().map(|(deadline, _)| *deadline).min()
                    {
                        receive
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .ok()
                    } else {
                        receive.recv().ok()
                    };
                    if let Some(item) = next {
                        if pending.len() < 128 {
                            pending.push(item);
                        } else if let Some(guard) = item.1.upgrade() {
                            guard.lock().expect("journal transfer lock poisoned").take();
                        }
                    }
                    for item in receive.try_iter() {
                        if pending.len() < 128 {
                            pending.push(item);
                        } else if let Some(guard) = item.1.upgrade() {
                            guard.lock().expect("journal transfer lock poisoned").take();
                        }
                    }
                    pending.retain(|(deadline, weak)| {
                        let Some(guard) = weak.upgrade() else {
                            return false;
                        };
                        if Instant::now() < *deadline {
                            return true;
                        }
                        guard.lock().expect("journal transfer lock poisoned").take();
                        false
                    });
                }
            });
            sender
        });
        if sender
            .try_send((deadline, Arc::downgrade(&transfer.0)))
            .is_err()
        {
            transfer.take();
        }
        transfer
    }
    pub fn take(&self) -> Option<JournalGuard> {
        self.0
            .lock()
            .expect("journal transfer lock poisoned")
            .take()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOutcome {
    Checked,
    Retained,
    HistoryUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCompletion {
    pub check: OrderCheck,
    pub outcome: OrderOutcome,
    pub reason: Option<RetainReason>,
    pub proven: BTreeSet<String>,
    pub journal: Option<Evaluation>,
    pub guard: Option<SharedJournalGuard>,
}

pub struct OrderJob {
    pub check: OrderCheck,
    pub deadline: Instant,
}

#[derive(Clone)]
pub struct OrderWorkerHandle {
    sender: mpsc::SyncSender<OrderJob>,
}
impl OrderWorkerHandle {
    pub fn submit(&self, job: OrderJob) -> bool {
        self.sender.try_send(job).is_ok()
    }
}

pub fn start_order_worker(
    env: BTreeMap<String, String>,
    normal_busy: Arc<dyn Fn() -> bool + Send + Sync>,
    live_sessions: Arc<dyn Fn() -> BTreeSet<(String, String)> + Send + Sync>,
    verify_owner: VerifyOwner,
    completion_ready: mpsc::SyncSender<()>,
    mutation_busy: MutationBusy,
) -> (OrderWorkerHandle, mpsc::Receiver<OrderCompletion>) {
    let (sender, receiver) = mpsc::sync_channel::<OrderJob>(2);
    let (completed, completions) = mpsc::sync_channel(3);
    std::thread::spawn(move || {
        let mut cursors = BTreeMap::<(String, String), Cursor>::new();
        loop {
            let job = receiver.recv_timeout(Duration::from_secs(1));
            let live = live_sessions();
            cursors.retain(|key, _| live.contains(key));
            let job = match job {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let key = (
                job.check.fence.home.clone(),
                job.check.fence.session.clone(),
            );
            let result =
                if !live.contains(&key) || (!cursors.contains_key(&key) && cursors.len() >= 4096) {
                    retained(
                        job.check,
                        OrderOutcome::HistoryUnknown,
                        RetainReason::HistoryUnknown,
                    )
                } else {
                    let cursor = cursors.entry(key).or_insert_with(|| {
                        Cursor::new(
                            job.check.fence.binding.locator.clone(),
                            job.check.fence.session.clone(),
                        )
                    });
                    execute_order_job(
                        &env,
                        &job,
                        cursor,
                        normal_busy.as_ref(),
                        verify_owner.as_ref(),
                        mutation_busy.as_ref(),
                    )
                };
            // A dropped completion releases its journal guard immediately.
            if completed.try_send(result).is_ok() {
                let _ = completion_ready.try_send(());
            }
        }
    });
    (OrderWorkerHandle { sender }, completions)
}

fn retained(check: OrderCheck, outcome: OrderOutcome, reason: RetainReason) -> OrderCompletion {
    OrderCompletion {
        check,
        outcome,
        reason: Some(reason),
        proven: BTreeSet::new(),
        journal: None,
        guard: None,
    }
}

fn execute_order_job(
    env: &BTreeMap<String, String>,
    job: &OrderJob,
    cursor: &mut Cursor,
    normal_busy: &(dyn Fn() -> bool + Send + Sync),
    verify_owner: &(dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync),
    mutation_busy: &(dyn Fn() -> bool + Send + Sync),
) -> OrderCompletion {
    let check = &job.check;
    if cursor.locator() != &check.fence.binding.locator
        || !check.fence.binding.executable.matches_embedded_process()
    {
        return retained(
            check.clone(),
            OrderOutcome::HistoryUnknown,
            RetainReason::HistoryUnknown,
        );
    }
    let mut remaining = REQUEST_BYTES;
    let mut attempts = 0;
    let mut next_attempt = Instant::now();
    loop {
        if Instant::now() >= job.deadline || remaining == 0 {
            return retained(
                check.clone(),
                OrderOutcome::Retained,
                if remaining == 0 {
                    RetainReason::ReaderBudget
                } else {
                    RetainReason::ReaderDeadline
                },
            );
        }
        if normal_busy() || Instant::now() < next_attempt {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let slice = match cursor.read_slice(remaining.min(SLICE_BYTES), job.deadline) {
            Ok(slice) => slice,
            Err(ReadFailure::HistoryUnknown) => {
                return retained(
                    check.clone(),
                    OrderOutcome::HistoryUnknown,
                    RetainReason::HistoryUnknown,
                );
            }
            Err(ReadFailure::Unavailable) => {
                return retained(
                    check.clone(),
                    OrderOutcome::Retained,
                    RetainReason::ReaderUnavailable,
                );
            }
        };
        remaining -= slice.bytes;
        if slice.more {
            continue;
        } // Yield to normal observation before every slice.
        attempts += 1;
        let proven = check
            .issued_turns
            .iter()
            .filter(|turn| cursor.proves(turn, &check.fence.turn))
            .cloned()
            .collect::<BTreeSet<_>>();
        if !slice.partial && !proven.is_empty() {
            let mut guard = match acquire_for_mutation(
                env,
                &check.fence,
                job.deadline,
                verify_owner,
                OwnerCheck::Full,
                Admission::WaitBeforeLock,
                mutation_busy,
            ) {
                Ok(guard) => guard,
                Err(reason) => {
                    return retained(
                        check.clone(),
                        if reason == RetainReason::Owner {
                            OrderOutcome::HistoryUnknown
                        } else {
                            OrderOutcome::Retained
                        },
                        reason,
                    );
                }
            };
            let evaluation = match guard.evaluate(Some(&check.fence.session), writer_state) {
                Ok(value) => value,
                Err(_) => {
                    return retained(
                        check.clone(),
                        OrderOutcome::Retained,
                        RetainReason::JournalEvaluation,
                    );
                }
            };
            if evaluation.epoch != check.fence.epoch {
                return retained(
                    check.clone(),
                    OrderOutcome::HistoryUnknown,
                    RetainReason::Epoch,
                );
            }
            if evaluation.veto {
                return retained(
                    check.clone(),
                    OrderOutcome::Retained,
                    RetainReason::JournalVeto,
                );
            }
            return OrderCompletion {
                check: check.clone(),
                outcome: OrderOutcome::Checked,
                reason: None,
                proven,
                journal: Some(evaluation),
                guard: Some(SharedJournalGuard::new(guard)),
            };
        }
        if attempts >= 3 {
            return retained(
                check.clone(),
                OrderOutcome::Retained,
                RetainReason::OrderUnproven,
            );
        }
        next_attempt = Instant::now() + Duration::from_millis(100);
    }
}

pub enum ProbeJob {
    RecoverHome {
        home: String,
        ticket: u64,
        deadline: Instant,
    },
    Sample {
        fence: Fence,
        probe_id: u64,
        deadline: Instant,
    },
    Commit {
        fence: Fence,
        through: u64,
        deadline: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeCompletion {
    RecoverHome {
        home: String,
        ticket: u64,
        epoch: Option<u64>,
    },
    Sample {
        fence: Fence,
        probe_id: u64,
        sample: Sample,
        failure: Option<RetainReason>,
        guard: Option<SharedJournalGuard>,
    },
    Commit {
        fence: Fence,
        through: u64,
        failure: Option<RetainReason>,
        guard: Option<SharedJournalGuard>,
    },
}

#[derive(Clone)]
pub struct ProbeWorkerHandle {
    sender: mpsc::SyncSender<ProbeJob>,
}
impl ProbeWorkerHandle {
    pub fn submit(&self, job: ProbeJob) -> bool {
        self.sender.try_send(job).is_ok()
    }
}

pub fn start_probe_workers(
    env: BTreeMap<String, String>,
    capture: super::CaptureCoordinatorHandle,
    verify_owner: VerifyOwner,
    completion_ready: mpsc::SyncSender<()>,
    mutation_busy: MutationBusy,
) -> (ProbeWorkerHandle, mpsc::Receiver<ProbeCompletion>) {
    let (sender, receiver) = mpsc::sync_channel::<ProbeJob>(2);
    let receiver = Arc::new(Mutex::new(receiver));
    let (complete, completions) = mpsc::sync_channel(4);
    for _ in 0..2 {
        let receiver = receiver.clone();
        let complete = complete.clone();
        let env = env.clone();
        let capture = capture.clone();
        let verify_owner = verify_owner.clone();
        let completion_ready = completion_ready.clone();
        let mutation_busy = mutation_busy.clone();
        std::thread::spawn(move || {
            loop {
                let Ok(job) = receiver.lock().expect("probe work lock poisoned").recv() else {
                    break;
                };
                let completion = match job {
                    ProbeJob::RecoverHome {
                        home,
                        ticket,
                        deadline,
                    } => {
                        let epoch = JournalLocation::new(&env, home.clone())
                            .ok()
                            .and_then(|location| location.lock(deadline).ok())
                            .and_then(|mut guard| guard.bump().ok().map(|()| guard.epoch()));
                        ProbeCompletion::RecoverHome {
                            home,
                            ticket,
                            epoch,
                        }
                    }
                    ProbeJob::Sample {
                        fence,
                        probe_id,
                        deadline,
                    } => {
                        let (sample, guard) = sample_with_guard(
                            &env,
                            &fence,
                            deadline,
                            verify_owner.as_ref(),
                            mutation_busy.as_ref(),
                            || {
                                capture.capture_question(
                                    fence.binding.pane.clone(),
                                    fence.binding.profile,
                                    deadline,
                                )
                            },
                        );
                        ProbeCompletion::Sample {
                            fence,
                            probe_id,
                            sample,
                            failure: guard.as_ref().err().copied().map(probe_reason),
                            guard: guard.ok(),
                        }
                    }
                    ProbeJob::Commit {
                        fence,
                        through,
                        deadline,
                    } => {
                        let guard = acquire_clean_guard(
                            &env,
                            &fence,
                            deadline,
                            verify_owner.as_ref(),
                            OwnerCheck::Full,
                            Admission::WaitBeforeLock,
                            mutation_busy.as_ref(),
                        );
                        ProbeCompletion::Commit {
                            fence,
                            through,
                            failure: guard.as_ref().err().copied().map(commit_reason),
                            guard: guard.ok(),
                        }
                    }
                };
                if complete.try_send(completion).is_ok() {
                    let _ = completion_ready.try_send(());
                }
            }
        });
    }
    (ProbeWorkerHandle { sender }, completions)
}

fn validate_probe_owner(
    fence: &Fence,
    deadline: Instant,
    verify: &(dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync),
    check: OwnerCheck,
) -> Result<(), RetainReason> {
    if Instant::now() >= deadline {
        return Err(RetainReason::Deadline);
    }
    if !verify(&fence.binding, check)
        || !fence.binding.executable.matches_embedded_process()
        || !fence.binding.locator.matches_current_file()
    {
        return Err(RetainReason::Owner);
    }
    Ok(())
}

fn sample_with_guard(
    env: &BTreeMap<String, String>,
    fence: &Fence,
    deadline: Instant,
    verify_owner: &(dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync),
    mutation_busy: &(dyn Fn() -> bool + Send + Sync),
    capture: impl FnOnce() -> Sample,
) -> (Sample, Result<SharedJournalGuard, RetainReason>) {
    if let Err(reason) = wait_for_mutation_idle(deadline, mutation_busy) {
        return (Sample::Ambiguous, Err(reason));
    }
    if let Err(reason) =
        validate_probe_owner(fence, deadline, verify_owner, OwnerCheck::CapturedPane)
    {
        return (Sample::Ambiguous, Err(reason));
    }
    let sample = capture();
    let guard = acquire_clean_guard(
        env,
        fence,
        deadline,
        verify_owner,
        OwnerCheck::CapturedPane,
        Admission::NoWaitAfterCapture,
        mutation_busy,
    );
    (
        if guard.is_ok() {
            sample
        } else {
            Sample::Ambiguous
        },
        guard,
    )
}

fn acquire_clean_guard(
    env: &BTreeMap<String, String>,
    fence: &Fence,
    deadline: Instant,
    verify_owner: &(dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync),
    check: OwnerCheck,
    admission: Admission,
    mutation_busy: &(dyn Fn() -> bool + Send + Sync),
) -> Result<SharedJournalGuard, RetainReason> {
    let mut guard = acquire_for_mutation(
        env,
        fence,
        deadline,
        verify_owner,
        check,
        admission,
        mutation_busy,
    )?;
    let evaluation = guard
        .evaluate(Some(&fence.session), writer_state)
        .map_err(|_| RetainReason::JournalEvaluation)?;
    if evaluation.veto {
        return Err(RetainReason::JournalVeto);
    }
    if evaluation.epoch != fence.epoch {
        return Err(RetainReason::Epoch);
    }
    Ok(SharedJournalGuard::new(guard))
}

fn acquire_for_mutation(
    env: &BTreeMap<String, String>,
    fence: &Fence,
    deadline: Instant,
    verify_owner: &(dyn Fn(&Binding, OwnerCheck) -> bool + Send + Sync),
    check: OwnerCheck,
    admission: Admission,
    mutation_busy: &(dyn Fn() -> bool + Send + Sync),
) -> Result<JournalGuard, RetainReason> {
    // A captured viewport must not age while waiting for admission. Order/commit
    // can retry acquisition, but bound expensive owner scans to three per job.
    let may_wait = admission == Admission::WaitBeforeLock;
    for _ in 0..if may_wait { 3 } else { 1 } {
        // Wait only in the IO worker and without a home lock, within the existing
        // job deadline. Every new attempt rechecks the live owner before flock.
        if may_wait {
            wait_for_mutation_idle(deadline, mutation_busy)?;
        } else if mutation_busy() {
            return Err(RetainReason::MutationBusy);
        }
        validate_probe_owner(fence, deadline, verify_owner, check)?;
        if mutation_busy() {
            continue;
        }
        let location = JournalLocation::new(env, fence.home.clone())
            .map_err(|_| RetainReason::JournalAcquire)?;
        let guard = location
            .lock(deadline)
            .map_err(|_| RetainReason::JournalAcquire)?;
        if mutation_busy() {
            drop(guard);
            continue;
        }
        return Ok(guard);
    }
    Err(RetainReason::MutationBusy)
}

fn wait_for_mutation_idle(
    deadline: Instant,
    mutation_busy: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), RetainReason> {
    while mutation_busy() {
        if Instant::now() >= deadline {
            return Err(RetainReason::MutationBusy);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn probe_reason(reason: RetainReason) -> RetainReason {
    match reason {
        RetainReason::Owner => RetainReason::ProbeOwner,
        RetainReason::Deadline => RetainReason::ProbeDeadline,
        RetainReason::JournalAcquire => RetainReason::ProbeJournalAcquire,
        RetainReason::JournalEvaluation => RetainReason::ProbeJournalEvaluation,
        RetainReason::JournalVeto => RetainReason::ProbeJournalVeto,
        RetainReason::Epoch => RetainReason::ProbeEpoch,
        other => other,
    }
}
fn commit_reason(reason: RetainReason) -> RetainReason {
    match reason {
        RetainReason::Owner => RetainReason::CommitOwner,
        RetainReason::Deadline => RetainReason::CommitDeadline,
        RetainReason::JournalAcquire => RetainReason::CommitJournalAcquire,
        RetainReason::JournalEvaluation => RetainReason::CommitJournalEvaluation,
        RetainReason::JournalVeto => RetainReason::CommitJournalVeto,
        RetainReason::Epoch => RetainReason::CommitEpoch,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_state::{AgentProcessIdentity, PaneInstance};
    use crate::question_notice::{
        ingress::TranscriptLocator,
        profile::{CodexProfile, ProfileRequest},
    };

    struct Fixture {
        root: std::path::PathBuf,
        child: std::process::Child,
        env: BTreeMap<String, String>,
        fence: Fence,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "vt-question-worker-{}",
                crate::pane_state::EventId::generate().unwrap().as_str()
            ));
            std::fs::create_dir_all(root.join("sessions")).unwrap();
            let path = root.join("sessions/transcript.jsonl");
            std::fs::write(&path, b"").unwrap();
            let child = crate::question_notice::profile::tests::ready_process();
            let process = AgentProcessIdentity {
                pid: child.id(),
                start_token: crate::daemon::lifecycle::agent_process_start_token(child.id())
                    .unwrap(),
            };
            let binding = Binding {
                owner: "synthetic-owner".into(),
                pane: PaneInstance {
                    pane_id: "%1".into(),
                    pane_pid: child.id(),
                },
                executable: ProfileRequest::capture(process.clone()).unwrap(),
                process,
                profile: CodexProfile::V01561,
                locator: TranscriptLocator::capture(&root, &path).unwrap(),
            };
            let fence = Fence {
                generation: 1,
                ingress: "synthetic".into(),
                home: binding.locator.home_digest(),
                session: crate::question_notice::turn_order::identifier_digest("session"),
                turn: crate::question_notice::turn_order::identifier_digest("b"),
                epoch: 0,
                acknowledged: 0,
                latest: 1,
                binding,
            };
            let env = BTreeMap::from([(
                "XDG_STATE_HOME".into(),
                root.join("state").to_string_lossy().into(),
            )]);
            let location = JournalLocation::new(&env, fence.home.clone()).unwrap();
            drop(
                location
                    .lock_hook(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
            );
            Self {
                root,
                child,
                env,
                fence,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn queued_guard_releases_lock_at_deadline_without_mutation_consuming_completion() {
        let f = Fixture::new();
        let location = JournalLocation::new(&f.env, f.fence.home.clone()).unwrap();
        let guard = location
            .lock(Instant::now() + Duration::from_millis(30))
            .unwrap();
        let transfer = SharedJournalGuard::new(guard);
        // This second writer can acquire only after the reaper drops the queued guard.
        let acquired = location
            .lock_hook(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(transfer.take().is_none());
        drop(acquired);
    }

    #[test]
    fn live_owner_change_before_capture_after_capture_and_before_commit_retains() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let f = Fixture::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        let live = AtomicBool::new(false);
        let checks = Mutex::new(Vec::new());
        let verify = |_: &Binding, check: OwnerCheck| {
            checks.lock().unwrap().push(check);
            live.load(Ordering::SeqCst)
        };
        let (sample, guard) =
            sample_with_guard(&f.env, &f.fence, deadline, &verify, &|| false, || {
                panic!("capture ran after owner changed")
            });
        assert_eq!(sample, Sample::Ambiguous);
        assert!(guard.is_err());
        live.store(true, Ordering::SeqCst);
        let (sample, guard) =
            sample_with_guard(&f.env, &f.fence, deadline, &verify, &|| false, || {
                live.store(false, Ordering::SeqCst);
                Sample::NormalComposer
            });
        assert_eq!(sample, Sample::Ambiguous);
        assert!(guard.is_err());
        assert!(
            acquire_clean_guard(
                &f.env,
                &f.fence,
                deadline,
                &verify,
                OwnerCheck::Full,
                Admission::WaitBeforeLock,
                &|| false
            )
            .is_err()
        );
        assert_eq!(
            *checks.lock().unwrap(),
            vec![
                OwnerCheck::CapturedPane,
                OwnerCheck::CapturedPane,
                OwnerCheck::CapturedPane,
                OwnerCheck::Full
            ]
        );
        live.store(true, Ordering::SeqCst);
        let (sample, guard) =
            sample_with_guard(&f.env, &f.fence, deadline, &verify, &|| false, || {
                Sample::NormalComposer
            });
        assert_eq!(sample, Sample::NormalComposer);
        assert!(guard.is_ok());
    }

    #[test]
    fn busy_mutation_waits_without_flock_and_rechecks_after_racing_acquisition() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let f = Fixture::new();
        let location = JournalLocation::new(&f.env, f.fence.home.clone()).unwrap();
        let verify = |_: &Binding, _: OwnerCheck| true;
        let occupied = location
            .lock_hook(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            acquire_clean_guard(
                &f.env,
                &f.fence,
                Instant::now() + Duration::from_millis(25),
                &verify,
                OwnerCheck::Full,
                Admission::WaitBeforeLock,
                &|| true
            )
            .unwrap_err(),
            RetainReason::MutationBusy
        );
        drop(occupied);
        let checks = AtomicUsize::new(0);
        let becomes_busy = || {
            let check = checks.fetch_add(1, Ordering::SeqCst);
            if check == 3 {
                // The previous acquired guard was dropped before waiting again.
                assert!(
                    location
                        .lock_hook(Instant::now() + Duration::from_secs(1))
                        .is_ok()
                );
            }
            check == 2
        };
        let guard = acquire_clean_guard(
            &f.env,
            &f.fence,
            Instant::now() + Duration::from_secs(2),
            &verify,
            OwnerCheck::Full,
            Admission::WaitBeforeLock,
            &becomes_busy,
        )
        .unwrap();
        assert!(checks.load(Ordering::SeqCst) >= 6);
        assert!(guard.take().is_some());
    }

    #[test]
    fn sample_waits_before_capture_but_never_waits_or_recaptures_afterward() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let f = Fixture::new();
        let verify = |_: &Binding, _: OwnerCheck| true;
        let (_, result) = sample_with_guard(
            &f.env,
            &f.fence,
            Instant::now() + Duration::from_millis(20),
            &verify,
            &|| true,
            || panic!("captured while normal mutation stayed busy"),
        );
        assert_eq!(result.unwrap_err(), RetainReason::MutationBusy);
        let busy = AtomicBool::new(false);
        let (sample, result) = sample_with_guard(
            &f.env,
            &f.fence,
            Instant::now() + Duration::from_secs(2),
            &verify,
            &|| busy.load(Ordering::SeqCst),
            || {
                busy.store(true, Ordering::SeqCst);
                Sample::NormalComposer
            },
        );
        assert_eq!(sample, Sample::Ambiguous);
        assert_eq!(result.unwrap_err(), RetainReason::MutationBusy);
    }

    #[test]
    fn flapping_mutation_admission_bounds_fresh_owner_scans() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let f = Fixture::new();
        let scans = AtomicUsize::new(0);
        let checks = AtomicUsize::new(0);
        let verify = |_: &Binding, _: OwnerCheck| {
            scans.fetch_add(1, Ordering::SeqCst);
            true
        };
        let busy = || checks.fetch_add(1, Ordering::SeqCst) % 2 == 1;
        let result = acquire_clean_guard(
            &f.env,
            &f.fence,
            Instant::now() + Duration::from_secs(2),
            &verify,
            OwnerCheck::Full,
            Admission::WaitBeforeLock,
            &busy,
        );
        assert_eq!(result.unwrap_err(), RetainReason::MutationBusy);
        assert_eq!(scans.load(Ordering::SeqCst), 3);
    }
}
