//! Exercises the example's explicit codec with the reusable document and adapter.

use rca_core::Store;
use rca_example_persistent_counter::codec::CounterCodec;
use rca_persistence::{
    Action, Codec, Document, FileBackend, FileOptions, LoadStatus, Operation, ReceiptError, State,
    SyncMode,
};
use rca_tokio::TokioRuntime;
use std::io;

/// Owns the runtime that must outlive every admitted file operation.
fn host() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Uses the record's byte limit and a sync policy supported on native targets.
fn backend(path: &std::path::Path, host: &tokio::runtime::Runtime) -> FileBackend {
    FileBackend::new(
        path,
        FileOptions {
            maximum_bytes: 16,
            sync: SyncMode::File,
        },
        host.handle().clone(),
    )
    .unwrap()
}

// Changing the record or accepting malformed bytes could destroy unread snapshots.
#[test]
fn codec_uses_versioned_counter_records_and_rejects_corruption() {
    assert_eq!(
        CounterCodec.encode(&7).unwrap(),
        b"RCACTR01\x07\0\0\0\0\0\0\0"
    );
    assert_eq!(
        CounterCodec
            .decode(b"RCACTR01\xff\xff\xff\xff\xff\xff\xff\xff")
            .unwrap(),
        u64::MAX
    );
    for bytes in [
        b"".as_slice(),
        b"RCACTR0",
        b"RCACTR01",
        b"RCACTR01\x07",
        b"RCACTR02\x07\0\0\0\0\0\0\0",
        b"RCACTR01\x07\0\0\0\0\0\0\0extra",
    ] {
        assert_eq!(
            CounterCodec.decode(bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

// Closing before loading must not claim success or overwrite unread storage.
#[test]
fn unloaded_close_and_corrupt_load_preserve_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    std::fs::write(&path, b"RCACTR99unknown").unwrap();
    let host = host();
    host.block_on(async {
        let backend = backend(&path, &host);
        let (state, document) = Document::new(0, backend.clone(), CounterCodec);
        let store = Store::builder(state, document, TokioRuntime::current().unwrap())
            .build()
            .unwrap();
        let (close, receipt) = Action::close();
        let _ = store.send(close).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
        assert!(!store.read(State::is_closing).await.unwrap());
        store.send(Action::load()).await.unwrap().finish().await;
        assert_eq!(
            store.read(State::load_status).await.unwrap(),
            LoadStatus::Unloaded
        );
        let error = store
            .read(|state| state.failure().unwrap().clone())
            .await
            .unwrap();
        assert_eq!(error.operation(), Operation::Decode);
        assert_eq!(
            error.error().downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        store.send(Action::set(99)).await.unwrap().finish().await;
        let (close, receipt) = Action::close();
        let _ = store.send(close).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
        store.send(Action::abort()).await.unwrap().finish().await;
        backend.close().await;
        store.shutdown().await.unwrap();
    });
    assert_eq!(std::fs::read(&path).unwrap(), b"RCACTR99unknown");
}

// Skipping the receipt/backend ordering could report an uncommitted final revision.
#[test]
fn document_receipt_then_backend_close_then_shutdown_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let host = host();
    host.block_on(async {
        for (expected, value) in [(0, 7), (7, 12)] {
            let backend = backend(&path, &host);
            let (state, document) = Document::new(0, backend.clone(), CounterCodec);
            let store = Store::builder(state, document, TokioRuntime::current().unwrap())
                .build()
                .unwrap();
            store.send(Action::load()).await.unwrap().finish().await;
            assert_eq!(store.read(|state| *state.value()).await.unwrap(), expected);
            let _ = store.send(Action::set(value)).await.unwrap();
            let (close, receipt) = Action::close();
            let _ = store.send(close).await.unwrap();
            let outcome = receipt.await.unwrap();
            assert_eq!(outcome.revision, 1);
            assert!(outcome.closing);
            assert!(
                store
                    .read(|state| state.is_closed() && state.acknowledged_revision() == Some(1))
                    .await
                    .unwrap()
            );
            backend.close().await;
            assert_eq!(
                backend.save(vec![]).await.unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            let state = store.shutdown_into_state().await.unwrap();
            assert_eq!(*state.value(), value);
            assert_eq!(
                CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
                value
            );
        }
    });
}
