//! H06 durable Manager job journal acceptance tests.

use siderostat::manager::{
    JobJournal, JobKind, JobPersistence, JobPhase, ManagerJobError, ManagerJobStorePersistence,
    ManagerReleaseStore, ManagerRoot, PersistenceError,
};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

fn root(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("siderostat-h06-jobs-{tag}-{nanos}"))
}

fn persistent_journal(root: &std::path::Path) -> (Arc<Mutex<ManagerReleaseStore>>, JobJournal) {
    let store = Arc::new(Mutex::new(
        ManagerReleaseStore::open(ManagerRoot::explicit(root.to_path_buf()), "node-a")
            .expect("open store"),
    ));
    let persistence = Arc::new(ManagerJobStorePersistence::new(store.clone()));
    let journal = JobJournal::open(persistence).expect("open journal");
    (store, journal)
}

#[test]
fn job_transitions_survive_reopen_and_inflight_jobs_become_interrupted() {
    let root = root("restart");
    let (store, mut journal) = persistent_journal(&root);

    let succeeded = journal
        .enqueue(JobKind::Fetch, "source@1")
        .expect("enqueue success job");
    journal
        .set_progress(&succeeded, 40)
        .expect("persist progress");
    journal.succeed(&succeeded).expect("persist success");

    let failed = journal
        .enqueue(JobKind::Build, "source@1")
        .expect("enqueue failure job");
    journal
        .fail(&failed, "manager backend failed")
        .expect("persist failure");

    let cancelling = journal
        .enqueue(JobKind::Download, "model@1")
        .expect("enqueue cancelling job");
    journal
        .request_cancel(&cancelling)
        .expect("persist cancellation");

    let running = journal
        .enqueue(JobKind::Verify, "model@1")
        .expect("enqueue running job");
    journal
        .set_progress(&running, 37)
        .expect("persist progress");
    drop(journal);
    drop(store);

    let reopened_store = Arc::new(Mutex::new(
        ManagerReleaseStore::open(ManagerRoot::explicit(root.clone()), "node-a")
            .expect("reopen store"),
    ));
    let persistence = Arc::new(ManagerJobStorePersistence::new(reopened_store.clone()));
    let mut reopened = JobJournal::open(persistence).expect("recover journal");
    assert_eq!(
        reopened.get(&succeeded).expect("succeeded").phase,
        JobPhase::Succeeded
    );
    assert_eq!(reopened.get(&succeeded).expect("succeeded").progress, 100);
    assert_eq!(
        reopened.get(&failed).expect("failed").phase,
        JobPhase::Failed
    );
    assert_eq!(
        reopened.get(&cancelling).expect("cancelled").phase,
        JobPhase::Interrupted
    );
    assert_eq!(
        reopened.get(&running).expect("interrupted").phase,
        JobPhase::Interrupted
    );
    assert_eq!(reopened.get(&running).expect("interrupted").progress, 37);
    assert_eq!(
        reopened.succeed(&running),
        Err(ManagerJobError::InvalidTransition)
    );
    assert_eq!(
        reopened.get(&running).expect("interrupted").phase,
        JobPhase::Interrupted
    );
    assert!(
        !reopened
            .succeed_if_running(&running)
            .expect("late success guard")
    );
    assert_eq!(
        reopened.get(&running).expect("interrupted").phase,
        JobPhase::Interrupted
    );

    let retry = reopened
        .enqueue(JobKind::Verify, "model@1")
        .expect("retry interrupted request");
    assert_ne!(
        retry, running,
        "interrupted jobs do not hold a running dedup key"
    );
    let persisted = reopened_store
        .lock()
        .expect("store lock")
        .snapshot()
        .jobs
        .clone();
    assert_eq!(persisted[&running].phase, JobPhase::Interrupted);
}

#[derive(Default)]
struct FailingPersistence {
    jobs: Mutex<(BTreeMap<String, siderostat::manager::ManagerJob>, u64)>,
    fail_save: AtomicBool,
}

impl JobPersistence for FailingPersistence {
    fn load(&self) -> Result<(Vec<siderostat::manager::ManagerJob>, u64), PersistenceError> {
        let jobs = self.jobs.lock().expect("jobs lock");
        Ok((jobs.0.values().cloned().collect(), jobs.1))
    }

    fn save(
        &self,
        records: &[siderostat::manager::ManagerJob],
        next_id: u64,
    ) -> Result<(), PersistenceError> {
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(PersistenceError::new("injected save failure"));
        }
        let mut jobs = self.jobs.lock().expect("jobs lock");
        jobs.0 = records
            .iter()
            .cloned()
            .map(|job| (job.id.clone(), job))
            .collect();
        jobs.1 = next_id;
        Ok(())
    }
}

#[test]
fn failed_terminal_write_does_not_publish_success_or_enqueue_memory_state() {
    let persistence = Arc::new(FailingPersistence::default());
    let mut journal = JobJournal::open(persistence.clone()).expect("open journal");
    let id = journal
        .enqueue(JobKind::Build, "source@1")
        .expect("enqueue");
    persistence.fail_save.store(true, Ordering::SeqCst);

    assert_eq!(
        journal.succeed_if_running(&id),
        Err(ManagerJobError::Persistence)
    );
    assert_eq!(journal.get(&id).expect("job").phase, JobPhase::Running);
    assert_eq!(
        journal.enqueue(JobKind::Build, "other-source@2"),
        Err(ManagerJobError::Persistence)
    );
    assert_eq!(journal.all().len(), 1);

    persistence.fail_save.store(false, Ordering::SeqCst);
    journal.succeed_if_running(&id).expect("persist success");
    assert_eq!(journal.get(&id).expect("job").phase, JobPhase::Succeeded);
}
