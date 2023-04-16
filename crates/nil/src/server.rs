use crate::capabilities::server_capabilities;
use crate::config::{Config, CONFIG_KEY};
use crate::{convert, handler, lsp_ext, UrlExt, Vfs, MAX_FILE_LEN};
use anyhow::{bail, Context, Result};
use async_lsp::router::Router;
use async_lsp::{ClientSocket, ErrorCode, LanguageClient, ResponseError};
use ide::{Analysis, AnalysisHost, Cancelled, FlakeInfo, VfsPath};
use lsp_types::request::{self as req, Request};
use lsp_types::{
    notification as notif, ConfigurationItem, ConfigurationParams, Diagnostic,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, InitializeParams, InitializeResult, InitializedParams, MessageType,
    PublishDiagnosticsParams, ServerInfo, ShowMessageParams, Url,
};
use nix_interop::nixos_options::{self, NixosOptions};
use nix_interop::{flake_lock, FLAKE_FILE, FLAKE_LOCK_FILE};
use std::backtrace::Backtrace;
use std::borrow::BorrowMut;
use std::cell::Cell;
use std::collections::HashMap;
use std::future::{ready, Future};
use std::io::ErrorKind;
use std::ops::ControlFlow;
use std::panic::UnwindSafe;
use std::sync::{Arc, Once, RwLock};
use std::{fmt, panic};
use tokio::task::JoinHandle;
use tokio::{fs, task};

const LSP_SERVER_NAME: &str = "nil";

const NIXOS_OPTIONS_FLAKE_INPUT: &str = "nixpkgs";

type NotifyResult = ControlFlow<async_lsp::Result<()>>;

type Task = Box<dyn FnOnce() -> Event + Send + 'static>;

enum Event {
    Diagnostics {
        uri: Url,
        version: u64,
        diagnostics: Vec<Diagnostic>,
    },
}

struct UpdateConfigEvent(serde_json::Value);
struct SetFlakeInfoEvent(Option<FlakeInfo>);
struct SetNixosOptionsEvent(NixosOptions);

pub struct Server {
    // States.
    /// This contains an internal RWLock and must not lock together with `vfs`.
    host: AnalysisHost,
    vfs: Arc<RwLock<Vfs>>,
    opened_files: HashMap<Url, FileData>,
    config: Arc<Config>,
    /// Monotonic version counter for diagnostics calculation ordering.
    version_counter: u64,
    /// Tried to load flake?
    /// This is used to reload flake only once after the configuration is first loaded.
    tried_flake_load: bool,

    // Ongoing tasks.
    load_flake_workspace_fut: Option<JoinHandle<()>>,

    // Client socket.
    client: ClientSocket,
}

#[derive(Debug, Default)]
struct FileData {
    diagnostics_version: u64,
    diagnostics: Vec<Diagnostic>,
}

impl Server {
    pub fn new_router(client: ClientSocket) -> Router<Self> {
        let this = Self::new(client);
        let mut router = Router::new(this);
        router
            //// Lifecycle ////
            .request::<req::Initialize, _>(Self::on_initialize)
            .notification::<notif::Initialized>(Self::on_initialized)
            .request::<req::Shutdown, _>(|_, _| ready(Ok(())))
            .notification::<notif::Exit>(|_, _| ControlFlow::Break(Ok(())))
            //// Notifications ////
            .notification::<notif::DidOpenTextDocument>(Self::on_did_open)
            .notification::<notif::DidCloseTextDocument>(Self::on_did_close)
            .notification::<notif::DidChangeTextDocument>(Self::on_did_change)
            .notification::<notif::DidChangeConfiguration>(Self::on_did_change_configuration)
            // Workaround:
            // > In former implementations clients pushed file events without the server actively asking for it.
            // Ref: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_didChangeWatchedFiles
            .notification::<notif::DidChangeWatchedFiles>(|_, _| ControlFlow::Continue(()))
            //// Requests ////
            .request_snap::<req::GotoDefinition>(handler::goto_definition)
            .request_snap::<req::References>(handler::references)
            .request_snap::<req::Completion>(handler::completion)
            .request_snap::<req::SelectionRangeRequest>(handler::selection_range)
            .request_snap::<req::PrepareRenameRequest>(handler::prepare_rename)
            .request_snap::<req::Rename>(handler::rename)
            .request_snap::<req::SemanticTokensFullRequest>(handler::semantic_token_full)
            .request_snap::<req::SemanticTokensRangeRequest>(handler::semantic_token_range)
            .request_snap::<req::HoverRequest>(handler::hover)
            .request_snap::<req::DocumentSymbolRequest>(handler::document_symbol)
            .request_snap::<req::Formatting>(handler::formatting)
            .request_snap::<req::DocumentLinkRequest>(handler::document_links)
            .request_snap::<req::CodeActionRequest>(handler::code_action)
            .request_snap::<req::DocumentHighlightRequest>(handler::document_highlight)
            .request_snap::<lsp_ext::ParentModule>(handler::parent_module)
            //// Events ////
            .event(Self::on_set_flake_info)
            .event(Self::on_set_nixos_options)
            .event(Self::on_update_config)
            // TODO: Use individual event types instead.
            .event(Self::on_event);
        router
    }

    pub fn new(client: ClientSocket) -> Self {
        Self {
            host: AnalysisHost::default(),
            vfs: Arc::new(RwLock::new(Vfs::new())),
            opened_files: HashMap::default(),
            // Will be set during initialization.
            config: Arc::new(Config::new("/non-existing-path".into())),
            version_counter: 0,
            tried_flake_load: false,

            load_flake_workspace_fut: None,

            client,
        }
    }

    // TODO: Refactor blocking tasks into async tasks as possible.
    fn spawn_task(&self, task: Task) {
        let client = self.client.clone();
        task::spawn(async move {
            let ret: Event = task::spawn_blocking(task).await.expect("Task panicked");
            let _: Result<_, _> = client.emit(ret);
        });
    }

    fn on_initialize(
        &mut self,
        params: InitializeParams,
    ) -> impl Future<Output = Result<InitializeResult, ResponseError>> {
        tracing::info!("Init params: {params:?}");

        // TODO: Use `workspaceFolders`.
        let root_path = match params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
        {
            Some(path) => path,
            None => std::env::current_dir().expect("Failed to the current directory"),
        };

        *Arc::get_mut(&mut self.config).expect("No concurrent access yet") = Config::new(root_path);

        ready(Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: LSP_SERVER_NAME.into(),
                version: option_env!("CFG_RELEASE").map(Into::into),
            }),
        }))
    }

    fn on_initialized(&mut self, _params: InitializedParams) -> NotifyResult {
        // Load configurations before loading flake.
        // The latter depends on `nix.binary`.
        self.spawn_reload_config();

        ControlFlow::Continue(())
    }

    fn on_did_open(&mut self, params: DidOpenTextDocumentParams) -> NotifyResult {
        // Ignore the open event for unsupported files, thus all following interactions
        // will error due to unopened files.
        let len = params.text_document.text.len();
        if len > MAX_FILE_LEN {
            self.client.show_message_ext(
                MessageType::WARNING,
                "Disable LSP functionalities for too large file ({len} > {MAX_FILE_LEN})",
            );
            return ControlFlow::Continue(());
        }

        let uri = &params.text_document.uri;
        self.set_vfs_file_content(uri, params.text_document.text);
        self.opened_files.insert(uri.clone(), FileData::default());
        ControlFlow::Continue(())
    }

    fn on_did_close(&mut self, params: DidCloseTextDocumentParams) -> NotifyResult {
        // N.B. Don't clear text here.
        self.opened_files.remove(&params.text_document.uri);
        ControlFlow::Continue(())
    }

    fn on_did_change(&mut self, params: DidChangeTextDocumentParams) -> NotifyResult {
        let mut vfs = self.vfs.write().unwrap();
        let uri = &params.text_document.uri;
        // Ignore files not maintained in Vfs.
        let Ok(file) = vfs.file_for_uri(uri) else { return ControlFlow::Continue(()) };
        for change in params.content_changes {
            let ret = (|| {
                let del_range = match change.range {
                    None => None,
                    Some(range) => Some(convert::from_range(&vfs, file, range).ok()?.1),
                };
                vfs.change_file_content(file, del_range, &change.text)
                    .ok()?;
                Some(())
            })();
            if ret.is_none() {
                tracing::error!(
                    "File is out of sync! Failed to apply change for {uri}: {change:?}"
                );

                // Clear file states to minimize pollution of the broken state.
                self.opened_files.remove(uri);
                // TODO: Remove the file from Vfs.
            }
        }
        drop(vfs);
        // FIXME: This blocks.
        self.apply_vfs_change();
        ControlFlow::Continue(())
    }

    fn on_did_change_configuration(
        &mut self,
        _params: DidChangeConfigurationParams,
    ) -> NotifyResult {
        // As stated in https://github.com/microsoft/language-server-protocol/issues/676,
        // this notification's parameters should be ignored and the actual config queried separately.
        self.spawn_reload_config();
        ControlFlow::Continue(())
    }

    fn on_event(&mut self, event: Event) -> NotifyResult {
        match event {
            Event::Diagnostics {
                uri,
                version,
                diagnostics,
            } => match self.opened_files.get_mut(&uri) {
                Some(f) if f.diagnostics_version < version => {
                    f.diagnostics_version = version;
                    f.diagnostics = diagnostics.clone();
                    tracing::trace!(
                        "Push {} diagnostics of {uri}, version {version}",
                        diagnostics.len(),
                    );
                    task::spawn({
                        let mut client = self.client.clone();
                        async move {
                            client.publish_diagnostics(PublishDiagnosticsParams {
                                uri,
                                diagnostics,
                                version: None,
                            })
                        }
                    });
                }
                _ => tracing::debug!("Ignore raced diagnostics of {uri}, version {version}"),
            },
        }
        ControlFlow::Continue(())
    }

    /// Spawn a task to (re)load the flake workspace via `flake.{nix,lock}`, including flake info,
    /// NixOS options and outputs (TODO).
    fn spawn_load_flake_workspace(&mut self) {
        let fut = task::spawn(Self::load_flake_workspace(
            self.vfs.clone(),
            self.config.clone(),
            self.client.clone(),
        ));
        if let Some(prev_fut) = self.load_flake_workspace_fut.replace(fut) {
            prev_fut.abort();
        }
    }

    async fn load_flake_workspace(
        vfs: Arc<RwLock<Vfs>>,
        config: Arc<Config>,
        mut client: ClientSocket,
    ) {
        tracing::info!("Loading flake workspace");

        let flake_info = match Self::load_flake_info(&vfs, &config).await {
            Ok(ret) => {
                let _: Result<_, _> = client.emit(SetFlakeInfoEvent(ret.clone()));
                ret
            }
            Err(err) => {
                client.show_message_ext(
                    MessageType::ERROR,
                    format!("Failed to load flake workspace: {err:#}"),
                );
                return;
            }
        };
        let Some(flake_info) = flake_info else { return };

        if flake_info
            .input_store_paths
            .values()
            .any(|path| !path.as_path().expect("Must be real paths").exists())
        {
            // TODO: Run it.
            client.show_message_ext(
                MessageType::WARNING,
                "Some flake inputs are not available, please run `nix flake archive` to fetch all inputs",
            );
            return;
        }

        // TODO: A better way to retrieve the nixpkgs for options?
        if let Some(nixpkgs_path) = flake_info
            .input_store_paths
            .get(NIXOS_OPTIONS_FLAKE_INPUT)
            .and_then(VfsPath::as_path)
        {
            tracing::info!("Evaluating NixOS options from {}", nixpkgs_path.display());

            // TODO: Async process.
            let ret = task::spawn_blocking({
                let nixpkgs_path = nixpkgs_path.to_owned();
                move || nixos_options::eval_all_options(&config.nix_binary, &nixpkgs_path)
            })
            .await
            .expect("Panicked while evaluting NixOS options")
            .context("Failed to evaluate NixOS options");
            match ret {
                // Sanity check.
                Ok(opts) if !opts.is_empty() => {
                    tracing::info!("Loaded NixOS options ({} top-level options)", opts.len());
                    let _: Result<_, _> = client.emit(SetNixosOptionsEvent(opts));
                }
                Ok(_) => tracing::error!("Empty NixOS options?"),
                Err(err) => {
                    client.show_message_ext(MessageType::ERROR, format_args!("{err:#}"));
                }
            }
        }
    }

    async fn load_flake_info(vfs: &RwLock<Vfs>, config: &Config) -> Result<Option<FlakeInfo>> {
        tracing::info!("Loading flake info");

        let flake_path = config.root_path.join(FLAKE_FILE);
        let lock_path = config.root_path.join(FLAKE_LOCK_FILE);

        let flake_vpath = VfsPath::new(&flake_path);
        let flake_src = match fs::read_to_string(&flake_path).await {
            Ok(src) => src,
            // Not a flake.
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(None);
            }
            // Read failure.
            Err(err) => {
                return Err(anyhow::Error::new(err)
                    .context(format!("Failed to read flake root {flake_path:?}")));
            }
        };

        // Load the flake file in Vfs.
        let flake_file = {
            let mut vfs = vfs.write().unwrap();
            match vfs.file_for_path(&flake_vpath) {
                // If the file is already opened (transferred from client),
                // prefer the managed one. It contains more recent unsaved changes.
                Ok(file) => file,
                // Otherwise, cache the file content from disk.
                Err(_) => vfs.set_path_content(flake_vpath, flake_src),
            }
        };

        let lock_src = match fs::read(&lock_path).await {
            Ok(lock_src) => lock_src,
            // Flake without inputs has no lock file.
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(Some(FlakeInfo {
                    flake_file,
                    input_store_paths: HashMap::new(),
                }));
            }
            Err(err) => {
                return Err(anyhow::Error::new(err)
                    .context(format!("Failed to read flake lock {lock_path:?}")));
            }
        };

        let inputs = task::spawn_blocking({
            let nix_binary = config.nix_binary.clone();
            move || {
                // TODO: Async process.
                flake_lock::resolve_flake_locked_inputs(&nix_binary, &lock_src)
            }
        })
        .await
        .expect("Panicked while resolving flake lock")
        .context("Failed to resolve flake inputs from lock file")?;

        // We only need the map for input -> store path.
        let input_store_paths = inputs
            .into_iter()
            .map(|(key, input)| (key, VfsPath::new(input.store_path)))
            .collect();
        Ok(Some(FlakeInfo {
            flake_file,
            input_store_paths,
        }))
    }

    fn on_set_flake_info(&mut self, info: SetFlakeInfoEvent) -> NotifyResult {
        tracing::debug!("Set flake info: {:?}", info.0);
        self.vfs.write().unwrap().set_flake_info(info.0);
        self.apply_vfs_change();
        ControlFlow::Continue(())
    }

    fn on_set_nixos_options(&mut self, opts: SetNixosOptionsEvent) -> NotifyResult {
        tracing::debug!("Set NixOS options ({:?} top-levels)", opts.0.len());
        self.vfs.write().unwrap().set_nixos_options(opts.0);
        self.apply_vfs_change();
        ControlFlow::Continue(())
    }

    fn spawn_reload_config(&self) {
        let mut client = self.client.clone();
        tokio::spawn(async move {
            let ret = client
                .configuration(ConfigurationParams {
                    items: vec![ConfigurationItem {
                        scope_uri: None,
                        section: Some(CONFIG_KEY.into()),
                    }],
                })
                .await;
            let mut v = match ret {
                Ok(v) => v,
                Err(err) => {
                    client.show_message_ext(
                        MessageType::ERROR,
                        format_args!("Failed to update config: {err}"),
                    );
                    return;
                }
            };
            tracing::debug!("Updating config: {:?}", v);
            let v = v.pop().unwrap_or_default();
            let _: Result<_, _> = client.emit(UpdateConfigEvent(v));
        });
    }

    fn on_update_config(&mut self, value: UpdateConfigEvent) -> NotifyResult {
        let mut config = Config::clone(&self.config);
        let (errors, updated_diagnostics) = config.update(value.0);
        tracing::debug!("Updated config, errors: {errors:?}, config: {config:?}");
        self.config = Arc::new(config);

        if !errors.is_empty() {
            let msg = ["Failed to apply some settings:"]
                .into_iter()
                .chain(errors.iter().flat_map(|s| ["\n- ", s]))
                .collect::<String>();
            self.client.show_message_ext(MessageType::ERROR, msg);
        }

        // Refresh all diagnostics since the filter may be changed.
        if updated_diagnostics {
            let version = self.next_version();
            for uri in self.opened_files.keys() {
                tracing::trace!("Recalculate diagnostics of {uri}, version {version}");
                self.update_diagnostics(uri.clone(), version);
            }
        }

        // If this is the first load, load the flake workspace, which depends on `nix.binary`.
        if !self.tried_flake_load {
            self.tried_flake_load = true;
            // TODO: Register file watcher for flake.lock.
            self.spawn_load_flake_workspace();
        }

        ControlFlow::Continue(())
    }

    fn update_diagnostics(&self, uri: Url, version: u64) {
        let snap = self.snapshot();
        let task = move || {
            // Return empty diagnostics for ignored files.
            let diagnostics = (!snap.config.diagnostics_excluded_files.contains(&uri))
                .then(|| {
                    with_catch_unwind("diagnostics", || handler::diagnostics(snap, &uri))
                        .unwrap_or_else(|err| {
                            tracing::error!("Failed to calculate diagnostics: {err}");
                            Vec::new()
                        })
                })
                .unwrap_or_default();
            Event::Diagnostics {
                uri,
                version,
                diagnostics,
            }
        };
        self.spawn_task(Box::new(task));
    }

    fn next_version(&mut self) -> u64 {
        self.version_counter += 1;
        self.version_counter
    }

    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            analysis: self.host.snapshot(),
            vfs: Arc::clone(&self.vfs),
            config: Arc::clone(&self.config),
        }
    }

    fn set_vfs_file_content(&mut self, uri: &Url, text: String) {
        let vpath = uri.to_vfs_path();
        self.vfs.write().unwrap().set_path_content(vpath, text);
        self.apply_vfs_change();
    }

    fn apply_vfs_change(&mut self) {
        let changes = self.vfs.write().unwrap().take_change();
        tracing::trace!("Change: {:?}", changes);
        let file_changes = changes.file_changes.clone();

        // N.B. This acquires the internal write lock.
        // Must be called without holding the lock of `vfs`.
        self.host.apply_change(changes);

        let version = self.next_version();
        let vfs = self.vfs.read().unwrap();
        for (file, text) in file_changes {
            let uri = vfs.uri_for_file(file);
            if !self.opened_files.contains_key(&uri) {
                continue;
            }

            // FIXME: Removed or closed files are indistinguishable from empty files.
            if !text.is_empty() {
                self.update_diagnostics(uri, version);
            } else {
                // Clear diagnostics.
                let _: Result<_, _> = self.client.emit(Event::Diagnostics {
                    uri,
                    version,
                    diagnostics: Vec::new(),
                });
            }
        }
    }
}

trait RouterExt: BorrowMut<Router<Server>> {
    fn request_snap<R: Request>(
        &mut self,
        f: impl Fn(StateSnapshot, R::Params) -> Result<R::Result> + Send + Copy + UnwindSafe + 'static,
    ) -> &mut Self
    where
        R::Params: Send + UnwindSafe + 'static,
        R::Result: Send + 'static,
    {
        self.borrow_mut().request::<R, _>(move |this, params| {
            let snap = this.snapshot();
            async move {
                task::spawn_blocking(move || with_catch_unwind(R::METHOD, move || f(snap, params)))
                    .await
                    .expect("Already catch_unwind")
                    .map_err(error_to_response)
            }
        });
        self
    }
}

impl RouterExt for Router<Server> {}

trait ClientExt: BorrowMut<ClientSocket> {
    fn show_message_ext(&mut self, typ: MessageType, msg: impl fmt::Display) {
        // Maybe connect all tracing::* to LSP ShowMessage?
        let _: Result<_, _> = self.borrow_mut().show_message(ShowMessageParams {
            typ,
            message: msg.to_string(),
        });
    }
}

impl ClientExt for ClientSocket {}

fn with_catch_unwind<T>(ctx: &str, f: impl FnOnce() -> Result<T> + UnwindSafe) -> Result<T> {
    static INSTALL_PANIC_HOOK: Once = Once::new();
    thread_local! {
        static PANIC_LOCATION: Cell<String> = Cell::new(String::new());
    }

    INSTALL_PANIC_HOOK.call_once(|| {
        let old_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|loc| loc.to_string())
                .unwrap_or_default();
            let backtrace = Backtrace::force_capture();
            PANIC_LOCATION.with(|inner| {
                inner.set(format!("Location: {loc:#}\nBacktrace: {backtrace:#}"));
            });
            old_hook(info);
        }));
    });

    match panic::catch_unwind(f) {
        Ok(ret) => ret,
        Err(payload) => {
            let reason = payload
                .downcast_ref::<String>()
                .map(|s| &**s)
                .or_else(|| payload.downcast_ref::<&str>().map(|s| &**s))
                .unwrap_or("unknown");
            let mut loc = PANIC_LOCATION.with(|inner| inner.take());
            if loc.is_empty() {
                loc = "Location: unknown".into();
            }
            tracing::error!("Panicked in {ctx}: {reason}\n{loc}");
            bail!("Panicked in {ctx}: {reason}\n{loc}");
        }
    }
}

fn error_to_response(err: anyhow::Error) -> ResponseError {
    if err.is::<Cancelled>() {
        return ResponseError::new(ErrorCode::REQUEST_CANCELLED, "Client cancelled");
    }
    match err.downcast::<ResponseError>() {
        Ok(resp) => resp,
        Err(err) => ResponseError::new(ErrorCode::INTERNAL_ERROR, err),
    }
}

#[derive(Debug)]
pub struct StateSnapshot {
    pub(crate) analysis: Analysis,
    vfs: Arc<RwLock<Vfs>>,
    pub(crate) config: Arc<Config>,
}

impl StateSnapshot {
    pub(crate) fn vfs(&self) -> impl std::ops::Deref<Target = Vfs> + '_ {
        self.vfs.read().unwrap()
    }
}
