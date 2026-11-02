//! Real file-session joins; no revision or arbitrary-writer CAS claim.
#![cfg(feature = "file")]

use std::task::{Context, Waker};

use rca_persistence::{BackendSession, FileBackend, FileOptions, FileStage, SyncMode};

// Catches closing only an abandoned delivery future instead of the admitted
// blocking job, or undoing the admission fence when the close wait is dropped.
#[test]
fn abandoned_file_delivery_still_requires_physical_join_before_reload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("document");
    // Occupying the only blocking worker keeps the admitted file write queued
    // until the test explicitly releases it. No timing delay defines this order.
    let host = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let options = FileOptions {
        maximum_bytes: 64,
        sync: SyncMode::File,
    };
    let backend = FileBackend::new(&path, options, host.handle().clone()).unwrap();
    let alias = backend.clone();
    let (started, entered) = std::sync::mpsc::sync_channel(0);
    let (release, blocked) = std::sync::mpsc::sync_channel(0);
    let occupying_worker = host.spawn_blocking(move || {
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    entered.recv().unwrap();

    let mut writing = Box::pin(backend.save(vec![7, 8, 9]));
    let mut context = Context::from_waker(Waker::noop());
    assert!(writing.as_mut().poll(&mut context).is_pending());
    drop(writing); // Delivery is gone; the admitted external writer still exists.
    let mut closing = Box::pin(BackendSession::close(&backend));
    assert!(closing.as_mut().poll(&mut context).is_pending());
    drop(closing);
    let rejected = host.block_on(alias.load()).unwrap_err();
    assert_eq!(rejected.stage, FileStage::Admission);
    assert!(!path.exists());

    let mut joined = Box::pin(BackendSession::close(&alias));
    assert!(joined.as_mut().poll(&mut context).is_pending());
    release.send(()).unwrap();
    host.block_on(async {
        occupying_worker.await.unwrap();
        joined.await.unwrap();
    });
    assert_eq!(std::fs::read(&path).unwrap(), [7, 8, 9]);
    let restarted = FileBackend::new(&path, options, host.handle().clone()).unwrap();
    host.block_on(async {
        assert_eq!(restarted.load().await.unwrap(), Some(vec![7, 8, 9]));
        BackendSession::close(&restarted).await.unwrap();
    });
}
