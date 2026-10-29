//! Saves bounded bytes with same-directory replacement and explicit host ownership.
//!
//! One writer owns each target in a trusted, existing directory. Symlink and
//! regular-file checks are not protection against concurrent directory mutation.
//! Replacement does not preserve permissions, ACLs, ownership or hard-link
//! identity. There is no cross-process lock, network-filesystem or power-loss
//! guarantee, nor a claim of hardware-cache honesty or macOS `F_FULLFSYNC`.
//! Normal failure cleans owned temporary files; process death may leave orphans.
//! Loading ignores those orphans and never adopts or removes them.
//!
//! Dropping an operation future does not stop a running blocking job. After an
//! abandoned or ambiguous write, close the backend, then create a new document
//! and load afresh before editing. Backend close proves quiescence, not flush
//! success; document receipts describe persistence separately.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::Backend;

/// Selects the sync requests made before acknowledging replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncMode {
    /// Sync file contents, then replace, without syncing the directory entry.
    File,
    /// Additionally sync the parent directory; supported only on Unix.
    FileAndDirectory,
}

/// Bounds encoded payload size and selects explicit sync requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileOptions {
    /// Byte limit in `1..isize::MAX`, leaving room for a one-byte-over-limit vector.
    pub maximum_bytes: usize,
    /// Sync requests required for a successful save.
    pub sync: SyncMode,
}

/// Identifies failures, including ambiguous replacement boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileStage {
    /// Admitting an operation after the backend has closed.
    Admission,
    /// Options, path resolution, target validation or temporary-file creation.
    Prepare,
    /// Reading bytes, including an oversized input.
    Load,
    /// Writing bytes to the temporary file.
    Write,
    /// Syncing the temporary file before replacement.
    SyncFile,
    /// Replacing the target; an error does not imply rollback.
    Replace,
    /// Syncing the directory after replacement already occurred.
    SyncDirectory,
    /// Joining an offloaded operation; its commit status may be unknown.
    Join,
}

/// Retains the failed stage and original typed I/O or host error.
///
/// Display includes the original cause and a backtrace when Rust capture is enabled.
/// Set `RUST_LIB_BACKTRACE=1` before starting the process to capture file error origins.
#[derive(Debug)]
pub struct FileError {
    /// Failed operation boundary.
    pub stage: FileStage,
    /// Preserves the original failure for typed inspection through `Error::source`.
    source: Source,
    /// Records the failure's origin when Rust backtrace capture is enabled.
    backtrace: Backtrace,
}

/// Distinguishes native I/O failure from host cancellation or panic.
#[derive(Debug)]
enum Source {
    /// Keeps the operating system's category and any inner error.
    Io(io::Error),
    /// Keeps the host's cancellation or panic result.
    Join(tokio::task::JoinError),
}

impl FileError {
    /// Returns the I/O category, mapping cancelled host jobs to `Interrupted`.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        match &self.source {
            Source::Io(error) => error.kind(),
            Source::Join(error) if error.is_cancelled() => io::ErrorKind::Interrupted,
            Source::Join(_) => io::ErrorKind::Other,
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Source::Io(error) => write!(f, "file {:?} failed: {error}", self.stage)?,
            Source::Join(error) => write!(f, "file {:?} failed: {error}", self.stage)?,
        }
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl Error for FileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match &self.source {
            Source::Io(error) => error,
            Source::Join(error) => error,
        })
    }
}

/// Adds the failed operation stage without replacing its original I/O cause.
fn failed(stage: FileStage, source: io::Error) -> FileError {
    FileError {
        stage,
        source: Source::Io(source),
        backtrace: Backtrace::capture(),
    }
}

/// Owns the fixed, resolved target and byte limit for one backend session.
#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
struct FileStorage {
    /// Resolves the parent at construction, independently of later working directories.
    path: PathBuf,
    /// Applies the same bound and sync policy to every operation in the session.
    options: FileOptions,
}

impl FileStorage {
    /// Validates options and resolves the parent without opening the target file.
    fn new(path: impl AsRef<Path>, options: FileOptions) -> Result<Self, FileError> {
        if options.maximum_bytes == 0 || options.maximum_bytes >= isize::MAX as usize {
            return Err(failed(
                FileStage::Prepare,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "maximum bytes must permit a one-byte-over-limit vector",
                ),
            ));
        }

        #[cfg(not(unix))]
        if options.sync == SyncMode::FileAndDirectory {
            return Err(failed(
                FileStage::Prepare,
                io::Error::new(io::ErrorKind::Unsupported, "directory sync requires Unix"),
            ));
        }

        let path = path.as_ref();
        // Components discard terminal separators/dots: reject directory-only spelling.
        let spelling = path.as_os_str().as_encoded_bytes();
        let terminal = spelling.strip_suffix(b".").unwrap_or(spelling);
        if terminal.last().is_some_and(|byte| {
            #[cfg(windows)]
            {
                *byte == b'/' || *byte == b'\\'
            }
            #[cfg(not(windows))]
            {
                *byte == b'/'
            }
        }) {
            return Err(failed(
                FileStage::Prepare,
                io::Error::new(io::ErrorKind::InvalidInput, "target spells a directory"),
            ));
        }

        let name = path.file_name().ok_or_else(|| {
            failed(
                FileStage::Prepare,
                io::Error::new(io::ErrorKind::InvalidInput, "target needs a filename"),
            )
        })?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent
            .canonicalize()
            .map_err(|e| failed(FileStage::Prepare, e))?;

        Ok(Self {
            path: parent.join(name),
            options,
        })
    }

    /// Reads one regular file with a one-byte probe beyond the configured limit.
    fn load(&self) -> Result<Option<Vec<u8>>, FileError> {
        if !regular_or_missing(&self.path).map_err(|e| failed(FileStage::Load, e))? {
            return Ok(None);
        }
        let file = File::open(&self.path).map_err(|e| failed(FileStage::Load, e))?;
        read_bounded(file, self.options.maximum_bytes).map(Some)
    }

    /// Validates the byte limit before creating a replacement in the target directory.
    fn save(&self, bytes: &[u8]) -> Result<(), FileError> {
        // Reject before target inspection or replacement creation.
        if bytes.len() > self.options.maximum_bytes {
            return Err(failed(
                FileStage::Prepare,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "snapshot exceeds maximum bytes",
                ),
            ));
        }
        let target = NativeReplacement::new(self).map_err(|e| failed(FileStage::Prepare, e))?;
        replace(bytes, target)
    }
}

/// Rejects oversized input without reading more than one byte beyond its limit.
fn read_bounded(reader: impl Read, maximum_bytes: usize) -> Result<Vec<u8>, FileError> {
    // Construction validates vector and u64 probe arithmetic. Do not preallocate
    // the configured maximum: a small file with a large limit remains cheap.
    let mut bytes = Vec::new();
    reader
        .take((maximum_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| failed(FileStage::Load, e))?;
    if bytes.len() > maximum_bytes {
        return Err(failed(
            FileStage::Load,
            io::Error::new(io::ErrorKind::InvalidData, "file exceeds maximum bytes"),
        ));
    }
    Ok(bytes)
}

/// Distinguishes missing targets from regular files without following a final symlink.
fn regular_or_missing(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Separates native replacement stages so tests can fail each real I/O boundary.
trait Replacement: Write {
    /// Syncs the complete temporary file before the target changes.
    fn sync_file(&mut self) -> io::Result<()>;
    /// Consumes the temporary file and returns the remaining directory-sync work.
    fn replace(self) -> io::Result<impl SyncDirectory>;
}

/// Completes directory sync after replacement has consumed the temporary file.
trait SyncDirectory {
    /// Syncs the containing directory when the selected policy requires it.
    fn sync_directory(self) -> io::Result<()>;
}

impl SyncDirectory for Option<File> {
    fn sync_directory(self) -> io::Result<()> {
        self.as_ref().map_or(Ok(()), File::sync_all)
    }
}

/// Runs replacement in order and retains the first failed stage and cause.
fn replace(bytes: &[u8], mut target: impl Replacement) -> Result<(), FileError> {
    target
        .write_all(bytes)
        .map_err(|e| failed(FileStage::Write, e))?;
    target
        .sync_file()
        .map_err(|e| failed(FileStage::SyncFile, e))?;
    let directory = target
        .replace()
        .map_err(|e| failed(FileStage::Replace, e))?;
    directory
        .sync_directory()
        .map_err(|e| failed(FileStage::SyncDirectory, e))
}

/// Owns temporary-file cleanup until successful replacement consumes that ownership.
struct NativeReplacement<'a> {
    /// Removes its file on drop unless successful replacement consumes it.
    temporary: tempfile::NamedTempFile,
    /// Borrows the immutable target while this synchronous operation is active.
    path: &'a Path,
    /// Keeps the parent open only when directory sync was requested.
    directory: Option<File>,
}

impl<'a> NativeReplacement<'a> {
    /// Opens the sync directory and creates an owned file beside the target.
    fn new(storage: &'a FileStorage) -> io::Result<Self> {
        regular_or_missing(&storage.path)?;
        // Construction appends a validated filename to the canonical parent.
        let mut parent = storage.path.components();
        let _ = parent.next_back();
        let parent = parent.as_path();
        let directory = if storage.options.sync == SyncMode::FileAndDirectory {
            Some(File::open(parent)?)
        } else {
            None
        };
        Ok(Self {
            temporary: tempfile::NamedTempFile::new_in(parent)?,
            path: &storage.path,
            directory,
        })
    }
}

impl Write for NativeReplacement<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.temporary.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.temporary.flush()
    }
}

impl Replacement for NativeReplacement<'_> {
    fn sync_file(&mut self) -> io::Result<()> {
        self.temporary.as_file().sync_all()
    }

    fn replace(self) -> io::Result<impl SyncDirectory> {
        // Persist failure drops its returned temporary file; success closes the
        // replaced file before the directory is synced.
        drop(
            self.temporary
                .persist(self.path)
                .map_err(|failure| failure.error)?,
        );
        Ok(self.directory)
    }
}

/// Keeps public operation futures movable and owned without exposing their host state.
type FileFuture<T> = Pin<Box<dyn Future<Output = Result<T, FileError>> + Send>>;

/// Offloads file operations onto an explicitly selected Tokio host.
///
/// Clones share admission and quiescence; separately constructed backends do not.
/// Callers retain the single-writer rule. The backend never stops its shared host.
/// Load and save futures own their session and can outlive every backend value.
///
/// # Examples
///
/// ```
/// use rca_persistence::{FileBackend, FileOptions, SyncMode};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let directory = tempfile::tempdir()?; // An existing, application-owned directory.
/// let host = tokio::runtime::Builder::new_current_thread().build()?;
/// let backend = FileBackend::new(directory.path().join("snapshot"),
///     FileOptions { maximum_bytes: 1024, sync: SyncMode::File },
///     host.handle().clone())?;
/// host.block_on(async {
///     backend.save(vec![1, 2, 3]).await?;
///     assert_eq!(backend.load().await?, Some(vec![1, 2, 3]));
///     backend.close().await; // Quiescence, not a document flush.
///     Ok::<_, rca_persistence::FileError>(())
/// })?;
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct FileBackend {
    /// Selects the caller-owned blocking pool without owning the runtime itself.
    host: tokio::runtime::Handle,
    /// Shares immutable storage and actual-job ownership across clones and operations.
    session: Arc<FileSession>,
}

/// Serialises the permanent admission fence with active-job registration and completion.
#[derive(Debug, Default)]
struct JobState {
    /// Once true, no operation can acquire a job completion guard.
    closed: bool,
    /// Counts guards held by queued or running blocking jobs, not awaiting callers.
    active: usize,
}

/// Retains one resolved target until every backend owner and admitted job releases it.
#[derive(Debug)]
struct FileSession {
    /// Shared by reference instead of copying the path into each operation.
    storage: FileStorage,
    /// Couples admission with completion so close cannot miss an admitted job.
    state: Mutex<JobState>,
    /// Closing broadcasts permanent quiescence to every independent waiter.
    completed: async_channel::Sender<()>,
    /// All close callers wait for the same permanent channel closure.
    completion: async_channel::Receiver<()>,
}

/// Releases exactly one admission when its queued or running job ends.
struct JobCompletion(Arc<FileSession>);

impl Drop for JobCompletion {
    fn drop(&mut self) {
        let complete = {
            let mut state = self.0.state.lock().unwrap();
            state.active -= 1;
            state.closed && state.active == 0
        };
        if complete {
            self.0.completed.close();
        }
    }
}

impl FileBackend {
    /// Resolves the existing parent and binds jobs to the explicit host.
    ///
    /// # Errors
    /// Rejects zero/overflowing byte limits, directory-only paths, inaccessible
    /// parents and non-Unix directory sync. Does not read or change the target.
    pub fn new(
        path: impl AsRef<Path>,
        options: FileOptions,
        host: tokio::runtime::Handle,
    ) -> Result<Self, FileError> {
        let storage = FileStorage::new(path, options)?;
        let (completed, completion) = async_channel::bounded(1);
        Ok(Self {
            host,
            session: Arc::new(FileSession {
                storage,
                state: Mutex::new(JobState::default()),
                completed,
                completion,
            }),
        })
    }

    /// Loads bounded bytes, distinguishing missing files from empty payloads.
    ///
    /// Admission and offloading begin on first poll, not future construction.
    ///
    /// # Errors
    /// Returns closed admission, non-regular input, oversize, I/O or host failures.
    ///
    /// # Panics
    /// Polling panics if the admission lock was poisoned or its count overflows.
    pub fn load(
        &self,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, FileError>> + Send + 'static {
        self.offload(FileStorage::load)
    }

    /// Replaces encoded bytes and performs the selected sync requests.
    ///
    /// An error at replacement or later can leave new bytes visible. Cancellation
    /// does not roll back filesystem I/O. Oversize saves create no replacement.
    ///
    /// # Errors
    /// Returns closed admission, oversize, prepare/write/sync/replace or host failures.
    ///
    /// # Panics
    /// Polling panics if the admission lock was poisoned or its count overflows.
    pub fn save(
        &self,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), FileError>> + Send + 'static {
        self.offload(move |storage| storage.save(&bytes))
    }

    /// Fences admission across clones and waits for admitted blocking jobs.
    ///
    /// The first poll permanently closes admission. Unpolled operations are not
    /// admitted; cancelling this wait leaves the fence intact. Concurrent and
    /// later waits observe the same completion, including queued-job cancellation.
    /// This is quiescence, not a successful flush or recovery of abandoned results.
    /// Reload after abandoned/ambiguous writes before restarting. Independently
    /// constructed backends are not fenced. An indefinitely blocked filesystem
    /// operation also prevents this wait from completing.
    ///
    /// # Panics
    /// Panics if the internal admission lock was poisoned.
    pub async fn close(&self) {
        let complete = {
            let mut state = self.session.state.lock().unwrap();
            state.closed = true;
            state.active == 0
        };
        if complete {
            self.session.completed.close();
        }
        let _ = self.session.completion.recv().await;
    }

    /// Admits on first poll and transfers completion ownership into the blocking job.
    fn offload<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&FileStorage) -> Result<T, FileError> + Send + 'static,
    ) -> FileFuture<T> {
        let session = Arc::clone(&self.session);
        let host = self.host.clone();
        Box::pin(async move {
            {
                let mut state = session.state.lock().unwrap();
                if state.closed {
                    return Err(failed(
                        FileStage::Admission,
                        io::Error::from(io::ErrorKind::BrokenPipe),
                    ));
                }
                state.active = state
                    .active
                    .checked_add(1)
                    .expect("file job count overflow");
            };
            // Capture before spawning: dropping a queued closure releases admission.
            let completion = JobCompletion(session);
            host.spawn_blocking(move || {
                // Bind the whole guard before borrowing storage so job completion
                // remains after operation return or unwinding, not closure capture.
                let job = completion;
                operation(&job.0.storage)
            })
            .await
            .map_err(|error| FileError {
                stage: FileStage::Join,
                source: Source::Join(error),
                backtrace: Backtrace::capture(),
            })?
        })
    }
}

impl Backend for FileBackend {
    type Error = FileError;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, FileError>> + Send {
        Self::load(self)
    }

    fn save(&self, bytes: Vec<u8>) -> impl Future<Output = Result<(), FileError>> + Send {
        Self::save(self, bytes)
    }
}

#[cfg(test)]
mod tests;
