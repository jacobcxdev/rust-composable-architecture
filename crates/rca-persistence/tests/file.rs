//! Exercises the optional adapter through its public byte and document contracts.
#![cfg(feature = "file")]

use std::error::Error;
use std::io;

use rca_persistence::{Backend, Codec, FileBackend, FileOptions, FileStage, SyncMode};

/// Creates an isolated host for file work and bounded test waits.
fn host() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Selects the byte limit and file-only durability for each test.
fn options(maximum_bytes: usize) -> FileOptions {
    FileOptions {
        maximum_bytes,
        sync: SyncMode::File,
    }
}

// Missing validation would admit a zero bound or overflow the over-limit probe.
#[test]
fn zero_and_overflowing_limits_are_rejected_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    for limit in [0, usize::MAX, isize::MAX as usize] {
        // Limit validation precedes both unsupported sync and invalid paths.
        for sync in [SyncMode::File, SyncMode::FileAndDirectory] {
            for path in [dir.path().join("value"), std::path::PathBuf::new()] {
                let error = FileBackend::new(
                    path,
                    FileOptions {
                        maximum_bytes: limit,
                        sync,
                    },
                    host.handle().clone(),
                )
                .unwrap_err();
                assert_eq!(error.stage, FileStage::Prepare);
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
                assert_eq!(
                    source.to_string(),
                    "maximum bytes must permit a one-byte-over-limit vector"
                );
                assert!(error.to_string().starts_with(
                    "file Prepare failed: maximum bytes must permit a one-byte-over-limit vector"
                ));
            }
        }
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// Directory-only spellings and missing filenames must fail before file creation.
#[test]
fn directory_only_targets_are_rejected_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    for path in [
        dir.path().join("value/"),
        dir.path().join("value/."),
        dir.path().join("missing/value/"),
        dir.path().join("missing/value/."),
        std::path::PathBuf::new(),
    ] {
        let error = FileBackend::new(path, options(8), host.handle().clone()).unwrap_err();
        assert_eq!(error.stage, FileStage::Prepare);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .is_some()
        );
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// Backslashes are filename bytes on Unix, not terminal directory separators.
#[cfg(unix)]
#[test]
fn terminal_backslashes_preserve_literal_unix_filenames() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    host.block_on(async {
        for name in ["value\\", "value\\."] {
            let path = dir.path().join(name);
            let backend = FileBackend::new(&path, options(8), host.handle().clone()).unwrap();
            assert_eq!(backend.load().await.unwrap(), None);
            backend.save(b"literal".to_vec()).await.unwrap();
            assert_eq!(backend.load().await.unwrap(), Some(b"literal".to_vec()));
            assert_eq!(std::fs::read(&path).unwrap(), b"literal");
            assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
            backend.close().await;
        }
    });
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

// Windows backslashes must be rejected before resolving a nonexistent parent.
#[cfg(windows)]
#[test]
fn terminal_backslashes_are_rejected_before_parent_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    for name in ["missing\\value\\", "missing\\value\\."] {
        let error =
            FileBackend::new(dir.path().join(name), options(8), host.handle().clone()).unwrap_err();
        assert_eq!(error.stage, FileStage::Prepare);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.to_string(), "target spells a directory");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// Accepting an unresolved parent would defer construction failure or create directories.
#[test]
fn missing_parent_is_rejected_without_creating_directories() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("missing");
    let host = host();
    let error =
        FileBackend::new(parent.join("value"), options(8), host.handle().clone()).unwrap_err();
    assert_eq!(error.stage, FileStage::Prepare);
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert!(source.raw_os_error().is_some());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// Treating every metadata error as absence would hide an invalid parent path.
#[cfg(unix)]
#[test]
fn changed_parent_metadata_errors_are_not_missing_documents() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("value");
    let host = host();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(8), host.handle().clone()).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        std::fs::write(&parent, b"preserved").unwrap();

        let load_error = backend.load().await.unwrap_err();
        let save_error = backend.save(b"new".to_vec()).await.unwrap_err();
        for (error, stage) in [
            (load_error, FileStage::Load),
            (save_error, FileStage::Prepare),
        ] {
            assert_eq!(error.stage, stage);
            assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
            let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.kind(), io::ErrorKind::NotADirectory);
            assert!(source.raw_os_error().is_some());
        }
        backend.close().await;
    });
    assert_eq!(std::fs::read(&parent).unwrap(), b"preserved");
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>(),
        [parent]
    );
}

// Losing the sync directory must fail before replacement and leave recovery possible.
#[cfg(unix)]
#[test]
fn moved_sync_directory_preserves_bytes_and_allows_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let moved = dir.path().join("moved");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("value");
    let host = host();
    host.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let backend = FileBackend::new(
                &path,
                FileOptions {
                    maximum_bytes: 8,
                    sync: SyncMode::FileAndDirectory,
                },
                host.handle().clone(),
            )
            .unwrap();
            backend.save(b"old".to_vec()).await.unwrap();
            std::fs::rename(&parent, &moved).unwrap();

            let error = backend.save(b"new".to_vec()).await.unwrap_err();
            assert_eq!(error.stage, FileStage::Prepare);
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
            assert!(source.raw_os_error().is_some());
            assert!(!parent.exists());
            assert_eq!(std::fs::read(moved.join("value")).unwrap(), b"old");
            assert_eq!(
                std::fs::read_dir(&moved)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>(),
                [std::ffi::OsString::from("value")]
            );
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

            std::fs::rename(&moved, &parent).unwrap();
            backend.save(b"recovery".to_vec()).await.unwrap();
            assert_eq!(backend.load().await.unwrap(), Some(b"recovery".to_vec()));
            backend.close().await;
        })
        .await
        .expect("directory recovery exceeded the watchdog");
    });
    assert_eq!(std::fs::read(&path).unwrap(), b"recovery");
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

// Truncation or a fixed counter record would break arbitrary byte replacement/restart.
#[test]
fn byte_limits_include_empty_and_exact_boundary_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("value");
    let host = host();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(4), host.handle().clone()).unwrap();
        assert_eq!(backend.load().await.unwrap(), None);
        for bytes in [vec![], vec![0, 255, 17, 4]] {
            backend.save(bytes.clone()).await.unwrap();
            assert_eq!(backend.load().await.unwrap(), Some(bytes));
        }
        backend.close().await;
        let restarted = FileBackend::new(&path, options(4), host.handle().clone()).unwrap();
        assert_eq!(restarted.load().await.unwrap(), Some(vec![0, 255, 17, 4]));
        restarted.close().await;
    });
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

// Futures must retain their session after the last backend value is dropped.
#[test]
fn operation_futures_outlive_every_backend_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("value");
    let host = host();
    let backend = FileBackend::new(&path, options(4), host.handle().clone()).unwrap();

    let save = backend.save(vec![0, 255, 17, 4]);
    let load = backend.load();
    drop(backend);

    host.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            save.await.unwrap();
            assert_eq!(load.await.unwrap(), Some(vec![0, 255, 17, 4]));
        })
        .await
        .expect("detached file operations exceeded the watchdog");
    });

    assert_eq!(std::fs::read(&path).unwrap(), [0, 255, 17, 4]);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

// Trait forwarding must retain byte fidelity and the native close admission fence.
#[test]
fn backend_trait_round_trip_and_close_preserve_bytes_and_failure_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trait-value");
    let host = host();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(8), host.handle().clone()).unwrap();
        assert_eq!(Backend::load(&backend).await.unwrap(), None);
        Backend::save(&backend, vec![0, 255, 17, 0, 128, 4, 9, 10])
            .await
            .unwrap();
        assert_eq!(
            Backend::load(&backend).await.unwrap(),
            Some(vec![0, 255, 17, 0, 128, 4, 9, 10])
        );
        backend.close().await;
        let load_error = Backend::load(&backend).await.unwrap_err();
        let save_error = Backend::save(&backend, b"rejected".to_vec())
            .await
            .unwrap_err();
        for error in [load_error, save_error] {
            assert_eq!(error.stage, FileStage::Admission);
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            let cause = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            // Capture settings can append a backtrace after the exact error summary.
            assert_eq!(
                error.to_string().lines().next(),
                Some(format!("file Admission failed: {}", io::ErrorKind::BrokenPipe).as_str())
            );
            assert!(std::ptr::eq(
                cause,
                error.source().unwrap().downcast_ref::<io::Error>().unwrap()
            ));
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    });
    assert_eq!(
        std::fs::read(&path).unwrap(),
        [0, 255, 17, 0, 128, 4, 9, 10]
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>(),
        [path]
    );
}

// Removing the save bound would replace the old file or create a temporary file.
#[test]
fn oversized_save_preserves_target_and_creates_no_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("value");
    let host = host();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(4), host.handle().clone()).unwrap();
        backend.save(b"old".to_vec()).await.unwrap();
        let error = backend.save(b"large".to_vec()).await.unwrap_err();
        assert_eq!(error.stage, FileStage::Prepare);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
        // The size error wins before replacement creation in a now-missing parent.
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        assert_eq!(
            backend.save(vec![0; 5]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            backend.save(vec![0; 4]).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        backend.close().await;
    });
}

// Permission failures must remain typed load errors, not missing or empty documents.
#[cfg(unix)]
#[test]
#[ignore = "requires a Unix filesystem and runner that deny opening mode-000 regular files"]
fn host_qualified_unreadable_file_preserves_load_error_and_bytes() {
    use std::os::unix::fs::PermissionsExt;

    /// Restores the original permissions when the test returns or unwinds.
    struct RestorePermissions {
        /// Keeps access to the file after its path becomes unreadable.
        file: std::fs::File,
        /// Records the permissions to restore without reconstructing mode bits.
        original: std::fs::Permissions,
    }
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            // Also restore on assertion unwinding; normal restoration is checked below.
            let _ = self.file.set_permissions(self.original.clone());
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("value");
    std::fs::write(&path, b"preserved").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let original = file.metadata().unwrap().permissions();
    let restore = RestorePermissions { file, original };
    restore
        .file
        .set_permissions(std::fs::Permissions::from_mode(0o000))
        .unwrap();
    assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    let denied = std::fs::File::open(&path)
        .expect_err("this qualification requires native read-permission enforcement");
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    assert!(denied.raw_os_error().is_some());

    let host = host();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(16), host.handle().clone()).unwrap();
        let error = backend.load().await.unwrap_err();
        assert_eq!(error.stage, FileStage::Load);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(source.raw_os_error(), denied.raw_os_error());
        backend.close().await;
    });

    restore
        .file
        .set_permissions(restore.original.clone())
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"preserved");
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>(),
        [path]
    );
}

// An unbounded read or silent truncation could decode a prefix and overwrite corruption.
#[test]
fn oversized_load_is_rejected_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("value");
    let host = host();
    std::fs::write(&path, b"oversized").unwrap();
    host.block_on(async {
        let backend = FileBackend::new(&path, options(4), host.handle().clone()).unwrap();
        let error = backend.load().await.unwrap_err();
        assert_eq!(error.stage, FileStage::Load);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        backend.close().await;
    });
    assert_eq!(std::fs::read(&path).unwrap(), b"oversized");
}

/// Encodes one fixture value as eight little-endian bytes.
struct Binary;
impl Codec<u64> for Binary {
    type Error = io::Error;
    fn encode(&self, value: &u64) -> Result<Vec<u8>, io::Error> {
        Ok(value.to_le_bytes().to_vec())
    }
    fn decode(&self, bytes: &[u8]) -> Result<u64, io::Error> {
        bytes
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

/// Encodes one fixture value as decimal UTF-8 text.
struct Text;
impl Codec<u64> for Text {
    type Error = io::Error;
    fn encode(&self, value: &u64) -> Result<Vec<u8>, io::Error> {
        Ok(value.to_string().into_bytes())
    }
    fn decode(&self, bytes: &[u8]) -> Result<u64, io::Error> {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid number"))
    }
}

// A format-specific backend would reject one of these independently encoded snapshots.
#[test]
fn one_backend_accepts_two_distinct_explicit_codecs() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    host.block_on(async {
        let backend =
            FileBackend::new(dir.path().join("value"), options(8), host.handle().clone()).unwrap();
        backend.save(Binary.encode(&12).unwrap()).await.unwrap();
        assert_eq!(
            Binary
                .decode(&backend.load().await.unwrap().unwrap())
                .unwrap(),
            12
        );
        assert_eq!(
            std::fs::read(dir.path().join("value")).unwrap(),
            b"\x0c\0\0\0\0\0\0\0"
        );
        backend.save(Text.encode(&37).unwrap()).await.unwrap();
        assert_eq!(
            Text.decode(&backend.load().await.unwrap().unwrap())
                .unwrap(),
            37
        );
        assert_eq!(std::fs::read(dir.path().join("value")).unwrap(), b"37");
        backend.close().await;
    });
}

#[cfg(not(unix))]
#[test]
fn directory_sync_is_rejected_before_writing_on_non_unix() {
    let dir = tempfile::tempdir().unwrap();
    let host = host();
    for path in [dir.path().join("value"), std::path::PathBuf::new()] {
        let error = FileBackend::new(
            path,
            FileOptions {
                maximum_bytes: 8,
                sync: SyncMode::FileAndDirectory,
            },
            host.handle().clone(),
        )
        .unwrap_err();
        assert_eq!(error.stage, FileStage::Prepare);
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.to_string(), "directory sync requires Unix");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
