use std::{
    collections::HashSet,
    io,
    net::IpAddr,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use crossbeam_channel::Sender;
use memofs::Vfs;
use thiserror::Error;

use crate::{
    change_processor::{ChangeProcessor, RefreshReply, RefreshSummary},
    message_queue::MessageQueue,
    project::{Project, ProjectError},
    session_id::SessionId,
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstanceContext, InstanceSnapshot,
        PatchSet, RojoTree,
    },
    snapshot_middleware::snapshot_from_vfs,
};

/// Options controlling how a [`ServeSession`] is built.
#[derive(Debug, Clone, Copy)]
pub struct ServeSessionOptions {
    /// If `false`, the filesystem watcher is disabled and the tree will only
    /// update when the caller explicitly requests it via
    /// [`ServeSession::refresh`].
    pub watch: bool,
}

impl Default for ServeSessionOptions {
    fn default() -> Self {
        Self { watch: true }
    }
}

/// Contains all of the state for a Rojo serve session. A serve session is used
/// when we need to build a Rojo tree and possibly rebuild it when input files
/// change.
///
/// Nothing here is specific to any Rojo interface. Though the primary way to
/// interact with a serve session is Rojo's HTTP right now, there's no reason
/// why Rojo couldn't expose an IPC or channels-based API for embedding in the
/// future. `ServeSession` would be roughly the right interface to expose for
/// those cases.
pub struct ServeSession {
    /// The object responsible for listening to changes from the in-memory
    /// filesystem, applying them, updating the Roblox instance tree, and
    /// routing messages through the session's message queue to any connected
    /// clients.
    ///
    /// SHOULD BE DROPPED FIRST! ServeSession and ChangeProcessor communicate
    /// with eachother via channels. If ServeSession hangs up those channels
    /// before dropping the ChangeProcessor, its thread will panic with a
    /// RecvError, causing the main thread to panic on drop.
    ///
    /// Allowed to be unused because it has side effects when dropped.
    #[allow(unused)]
    change_processor: ChangeProcessor,

    /// When the serve session was started. Used only for user-facing
    /// diagnostics.
    start_time: Instant,

    /// The root project for the serve session.
    ///
    /// This will be defined if a folder with a `default.project.json` file was
    /// used for starting the serve session, or if the user specified a full
    /// path to a `.project.json` file.
    root_project: Project,

    /// A randomly generated ID for this serve session. It's used to ensure that
    /// a client doesn't begin connecting to a different server part way through
    /// an operation that needs to be atomic.
    session_id: SessionId,

    /// The tree of Roblox instances associated with this session that will be
    /// updated in real-time. This is derived from the session's VFS and will
    /// eventually be mutable to connected clients.
    tree: Arc<Mutex<RojoTree>>,

    /// An in-memory filesystem containing all of the files relevant for this
    /// live session.
    ///
    /// The main use for accessing it from the session is for debugging issues
    /// with Rojo's live-sync protocol.
    vfs: Arc<Vfs>,

    /// A queue of changes that have been applied to `tree` that affect clients.
    ///
    /// Clients to the serve session will subscribe to this queue either
    /// directly or through the HTTP API to be notified of mutations that need
    /// to be applied.
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,

    /// A channel to send mutation requests on. These will be handled by the
    /// ChangeProcessor and trigger changes in the tree.
    tree_mutation_sender: Sender<PatchSet>,

    /// A channel to ask the [`ChangeProcessor`] to re-snapshot the project
    /// from disk and push any diff to connected clients. Used to implement
    /// the manual-push refresh mode.
    refresh_request_sender: Sender<RefreshReply>,

    /// Whether the filesystem watcher was enabled for this session. Reported
    /// over the API for diagnostics.
    watch_enabled: bool,
}

impl ServeSession {
    /// Start a new serve session from the given in-memory filesystem and start
    /// path with default options (filesystem watcher enabled).
    pub fn new<P: AsRef<Path>>(vfs: Vfs, start_path: P) -> Result<Self, ServeSessionError> {
        Self::new_with_options(vfs, start_path, ServeSessionOptions::default())
    }

    /// Start a new serve session from the given in-memory filesystem and start
    /// path with explicit options.
    ///
    /// The project file is expected to be loaded out-of-band since it's
    /// currently loaded from the filesystem directly instead of through the
    /// in-memory filesystem layer.
    pub fn new_with_options<P: AsRef<Path>>(
        vfs: Vfs,
        start_path: P,
        options: ServeSessionOptions,
    ) -> Result<Self, ServeSessionError> {
        let start_path = start_path.as_ref();
        let start_time = Instant::now();

        log::trace!(
            "Starting new ServeSession at path {} (watch: {})",
            start_path.display(),
            options.watch,
        );

        // Disable the filesystem watcher if the caller asked us to. This must
        // happen before any reads occur, since memofs registers watches as
        // files are read.
        if !options.watch {
            vfs.set_watch_enabled(false);
        }

        let root_project = Project::load_initial_project(&vfs, start_path)?;

        let mut tree = RojoTree::new(InstanceSnapshot::new());

        let root_id = tree.get_root_id();

        let instance_context =
            InstanceContext::with_emit_legacy_scripts(root_project.emit_legacy_scripts);

        log::trace!("Generating snapshot of instances from VFS");
        let snapshot = snapshot_from_vfs(&instance_context, &vfs, start_path)?;

        log::trace!("Computing initial patch set");
        let patch_set = compute_patch_set(snapshot, &tree, root_id);

        log::trace!("Applying initial patch set");
        apply_patch_set(&mut tree, patch_set);

        let session_id = SessionId::new();
        let message_queue = MessageQueue::new();

        let tree = Arc::new(Mutex::new(tree));
        let message_queue = Arc::new(message_queue);
        let vfs = Arc::new(vfs);

        let (tree_mutation_sender, tree_mutation_receiver) = crossbeam_channel::unbounded();
        let (refresh_request_sender, refresh_request_receiver) = crossbeam_channel::unbounded();

        log::trace!("Starting ChangeProcessor");
        let change_processor = ChangeProcessor::start(
            Arc::clone(&tree),
            Arc::clone(&vfs),
            Arc::clone(&message_queue),
            tree_mutation_receiver,
            refresh_request_receiver,
        );

        Ok(Self {
            change_processor,
            start_time,
            session_id,
            root_project,
            tree,
            message_queue,
            tree_mutation_sender,
            refresh_request_sender,
            watch_enabled: options.watch,
            vfs,
        })
    }

    /// Trigger a manual refresh of the project from disk.
    ///
    /// Re-runs the snapshot pipeline for the root project, applies any diff
    /// to the in-memory tree, and pushes the result through the message queue
    /// so connected plugins receive it. Returns a summary of the patch.
    ///
    /// Safe to call regardless of whether the filesystem watcher is enabled,
    /// though its intended use is when the watcher is disabled.
    pub fn refresh(&self) -> Result<RefreshSummary, ServeSessionError> {
        let (reply_sender, reply_receiver) = crossbeam_channel::bounded::<RefreshSummary>(1);

        self.refresh_request_sender
            .send(reply_sender)
            .map_err(|err| ServeSessionError::Refresh {
                message: format!("change processor is no longer running: {}", err),
            })?;

        reply_receiver
            .recv()
            .map_err(|err| ServeSessionError::Refresh {
                message: format!("change processor closed reply channel: {}", err),
            })
    }

    /// Whether the filesystem watcher is currently enabled.
    pub fn watch_enabled(&self) -> bool {
        self.watch_enabled
    }

    pub fn tree_handle(&self) -> Arc<Mutex<RojoTree>> {
        Arc::clone(&self.tree)
    }

    pub fn tree(&self) -> MutexGuard<'_, RojoTree> {
        self.tree.lock().unwrap()
    }

    pub fn tree_mutation_sender(&self) -> Sender<PatchSet> {
        self.tree_mutation_sender.clone()
    }

    #[allow(unused)]
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn message_queue(&self) -> &MessageQueue<AppliedPatchSet> {
        &self.message_queue
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn project_name(&self) -> &str {
        self.root_project
            .name
            .as_ref()
            .expect("all top-level projects must have their name set")
    }

    pub fn project_port(&self) -> Option<u16> {
        self.root_project.serve_port
    }

    pub fn place_id(&self) -> Option<u64> {
        self.root_project.place_id
    }

    pub fn game_id(&self) -> Option<u64> {
        self.root_project.game_id
    }

    pub fn start_time(&self) -> Instant {
        self.start_time
    }

    pub fn serve_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.serve_place_ids.as_ref()
    }

    pub fn blocked_place_ids(&self) -> Option<&HashSet<u64>> {
        self.root_project.blocked_place_ids.as_ref()
    }

    pub fn serve_address(&self) -> Option<IpAddr> {
        self.root_project.serve_address
    }

    pub fn root_dir(&self) -> &Path {
        self.root_project.folder_location()
    }

    pub fn root_project(&self) -> &Project {
        &self.root_project
    }
}

#[derive(Debug, Error)]
pub enum ServeSessionError {
    #[error(transparent)]
    Io {
        #[from]
        source: io::Error,
    },

    #[error(transparent)]
    Project {
        #[from]
        source: ProjectError,
    },

    #[error("Refresh failed: {message}")]
    Refresh { message: String },

    #[error(transparent)]
    Other {
        #[from]
        source: anyhow::Error,
    },
}
