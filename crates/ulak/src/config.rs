//! Three-layer TOML configuration, merged by hand (~no framework):
//!
//!   1. global    ~/.config/ulak/config.toml — machine defaults
//!   2. project   <root>/ulak.toml           — project settings
//!   3. local     <root>/ulak.local.toml     — optional local overrides
//!
//! Later layers override earlier ones, field by field. Lists replace
//! (they do not append) — "sonraki katman ezer".
//!
//! GLOBAL means this LAYER — the file that belongs to this machine rather
//! than to a checkout. Docker's own globals, the argv words typed before a
//! subcommand (`-f`, `-p`, `--project-directory`), keep that name in
//! `invocation` and `cli` and are a different thing entirely. The two never
//! share an identifier: this concept is always spelled `global_config_*`
//! or carries the layer label, and the argv one is always plain `globals`.
//! The layer was called "personal" until the word appeared in user-facing
//! output beside `init`'s own "your global config" — one thing had two
//! names, and the file is the machine's, not a person's.
//!
//! The merged `[workspace] namespace` is resolved here too. When none is
//! configured, one stable client namespace is minted under the local state
//! root. It belongs to transported bytes only: Docker's project identity
//! remains Docker's own answer and is never prefixed with this value.
//!
//! A lifecycle consumer is the one exception to asking current inputs what a
//! project is called: the background service and Ulak's root management
//! commands may already name a declared stack. `pin_existing_stack` records
//! that historical `StackPin` — the daemon AND the Compose identity — beside
//! the merged config, never over it, and `compose_model_project_name` is the
//! single answer footprint resolution uses for `-p` and its cache key.
//! Ordinary Docker invocations and undeclared foreground commands have no
//! historical pin, so Compose's own `.env` / top-level `name:` / directory
//! cascade remains authoritative.
//!
//! `Project::destination()` is where the pin and the cascade meet, and it is
//! the only place. The pin wins — a `host` edit must not move a running
//! stack's background job to another daemon — but the answer it displaced
//! travels with it as `DestinationDrift`. The pin used to be written INTO
//! `config.host`, and a checkout whose server had been destroyed and replaced
//! then had two answers with no way to say so: `ulak config` printed the new
//! host while `doctor`, `status`, `sync` and `clean` in the same directory
//! timed out against the old one. Routing reads `dest`; reports read `drift`
//! and name both servers.
//!
//! The project and local layers live at the config home itself
//! (`<root>/ulak.toml`, `<root>/ulak.local.toml`). A file under
//! `<root>/.config/` is where Ulak's own global template once said they
//! lived, and this Ulak never reads it; `misplaced_layers` names such files
//! so `ulak config` can say that an edit there changes nothing.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::invocation::Invocation;
use crate::ui::{self, fail};

pub const PROJECT_CONFIG: &str = "ulak.toml";
pub const LOCAL_CONFIG: &str = "ulak.local.toml";

/// One raw TOML layer; every field optional so merge stays trivial.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayer {
    host: Option<String>,
    workspace: Option<RawWorkspace>,
    sync: Option<RawSync>,
    forward: Option<RawForward>,
    service: Option<RawService>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    namespace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSync {
    exclude: Option<Vec<String>>,
    include: Option<Vec<String>>,
    protect: Option<Vec<String>>,
    max_delete: Option<DeleteBudget>,
}

/// How many server-side deletions one sync may make without asking.
///
/// `unlimited` is a NAME, not a number chosen to be big enough. Reaching
/// the same place by writing 999999999 buried a real decision inside a
/// value that reads like a typo, and nothing downstream could tell the
/// two apart. Because it IS a decision, every surface that shows the
/// budget shows this one too: it must never be quietly in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBudget {
    Limit(u64),
    Unlimited,
}

impl DeleteBudget {
    /// Whether `count` deletions may proceed without asking a human.
    pub fn allows(self, count: usize) -> bool {
        match self {
            DeleteBudget::Unlimited => true,
            DeleteBudget::Limit(n) => count as u64 <= n,
        }
    }

    pub fn is_unlimited(self) -> bool {
        self == DeleteBudget::Unlimited
    }
}

pub const UNLIMITED: &str = "unlimited";

impl std::fmt::Display for DeleteBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeleteBudget::Unlimited => f.write_str(UNLIMITED),
            DeleteBudget::Limit(n) => write!(f, "{n}"),
        }
    }
}

impl std::str::FromStr for DeleteBudget {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        if s == UNLIMITED {
            return Ok(DeleteBudget::Unlimited);
        }
        s.parse().map(DeleteBudget::Limit).map_err(|_| ())
    }
}

/// Hand-written so a bad word is refused while the file is being read,
/// where it becomes the same "is not valid Ulak TOML" refusal every other
/// type error gets, instead of surviving as far as a sync.
impl<'de> Deserialize<'de> for DeleteBudget {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Budget;
        impl serde::de::Visitor<'_> for Budget {
            type Value = DeleteBudget;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a whole number of deletions, or \"{UNLIMITED}\"")
            }

            fn visit_u64<E: serde::de::Error>(
                self,
                v: u64,
            ) -> std::result::Result<DeleteBudget, E> {
                Ok(DeleteBudget::Limit(v))
            }

            fn visit_i64<E: serde::de::Error>(
                self,
                v: i64,
            ) -> std::result::Result<DeleteBudget, E> {
                u64::try_from(v)
                    .map(DeleteBudget::Limit)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(v), &self))
            }

            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<DeleteBudget, E> {
                v.parse()
                    .map_err(|()| E::invalid_value(serde::de::Unexpected::Str(v), &self))
            }
        }
        d.deserialize_any(Budget)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForward {
    auto: Option<bool>,
}

/// The background service's own settings. Measured before this existed:
/// writing the very line the docs recommended (`[service] auto = false`)
/// made `deny_unknown_fields` reject the file, and status / doctor /
/// sync / clean ALL exited 1 with "unknown field `service`". The main
/// switch for a service must never be the thing that breaks the CLI.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    auto: Option<bool>,
    notify: Option<bool>,
}

/// Merged, defaulted view the rest of the program consumes.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: Option<String>,
    pub workspace: WorkspaceCfg,
    pub sync: SyncCfg,
    pub forward: ForwardCfg,
    pub service: ServiceCfg,
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceCfg {
    /// `None` means use this client's persistent automatic namespace.
    pub namespace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncCfg {
    pub exclude: Vec<String>,
    pub include: Vec<String>,
    pub protect: Vec<String>,
    pub max_delete: DeleteBudget,
}

#[derive(Debug, Clone)]
pub struct ForwardCfg {
    pub auto: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceCfg {
    /// Master switch. `false` means ulak never runs anything in the
    /// background for you — every command still works by hand.
    pub auto: bool,
    /// Parsed and deliberately unread. It was going to gate a desktop
    /// notification; measured instead that `status.json` had exactly ONE
    /// reader, so the service's silence was not a missing CHANNEL but a
    /// missing sentence in the commands already being typed. That is
    /// where trouble is reported now — no `osascript`, no permission
    /// prompt, no dedup against a flapping link.
    ///
    /// It keeps parsing because removing it would make
    /// `deny_unknown_fields` reject a config that has the line, which is
    /// exactly the trap it would be walking into: the main switch for a
    /// service must never be the thing that breaks the CLI.
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: None,
            workspace: WorkspaceCfg::default(),
            sync: SyncCfg {
                exclude: vec![],
                // .env is usually gitignored but compose needs it on the
                // server — force-included unless the user overrides.
                include: vec![".env".into()],
                protect: vec![],
                max_delete: DeleteBudget::Limit(25),
            },
            forward: ForwardCfg { auto: true },
            service: ServiceCfg {
                auto: true,
                notify: true,
            },
        }
    }
}

impl Config {
    /// `global` is true only for ~/.config/ulak/config.toml. Most
    /// settings use the ordinary cascade; the background service switch
    /// remains machine-wide and is therefore read only from that layer.
    fn apply(&mut self, layer: RawLayer, global: bool, from: &Path, won: &mut Won) {
        if layer.host.is_some() {
            self.host = layer.host;
            won.set("host", from);
        }
        if let Some(workspace) = layer.workspace
            && workspace.namespace.is_some()
        {
            self.workspace.namespace = workspace.namespace;
            won.set("workspace.namespace", from);
        }
        if let Some(sync) = layer.sync {
            if let Some(v) = sync.exclude {
                self.sync.exclude = v;
                won.set("sync.exclude", from);
            }
            if let Some(v) = sync.include {
                self.sync.include = v;
                won.set("sync.include", from);
            }
            if let Some(v) = sync.protect {
                self.sync.protect = v;
                won.set("sync.protect", from);
            }
            if let Some(v) = sync.max_delete {
                self.sync.max_delete = v;
                won.set("sync.max_delete", from);
            }
        }
        if let Some(forward) = layer.forward
            && let Some(auto) = forward.auto
        {
            self.forward.auto = auto;
            won.set("forward.auto", from);
        }
        // Whether this MACHINE runs a background service is not a
        // per-project decision. Ignoring it silently would be its own
        // trap, so a project layer that carries [service] is told.
        if let Some(service) = layer.service {
            if global {
                if let Some(v) = service.auto {
                    self.service.auto = v;
                    won.set("service.auto", from);
                }
                if let Some(v) = service.notify {
                    self.service.notify = v;
                }
            } else {
                ui::warn(&format!(
                    "[service] in {} is ignored — it is only read from your global config",
                    from.display()
                ));
                if let Some(global_path) = global_config_path() {
                    ui::dim(&format!("move it to {}", global_path.display()));
                }
            }
        }
    }
}

/// The namespace and locator that together name one transported workspace.
///
/// The namespace separates clients which happen to use the same SSH account
/// and absolute checkout path. The id separates checkouts within one client.
/// Docker's stack identity is deliberately absent: it is destination plus
/// Compose project name, and changing that would stop Ulak being a drop-in
/// Docker client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceKey {
    namespace: String,
    id: String,
}

impl WorkspaceKey {
    fn resolve(config: &Config, identity_path: &Path) -> Result<WorkspaceKey> {
        let namespace = match config.workspace.namespace.as_deref() {
            Some(raw) => configured_namespace(raw)?,
            None => automatic_namespace()?,
        };
        Ok(Self::from_valid_namespace(namespace, identity_path))
    }

    pub(crate) fn from_namespace(raw: &str, identity_path: &Path) -> Result<WorkspaceKey> {
        let namespace = configured_namespace(raw)?;
        Ok(Self::from_valid_namespace(namespace, identity_path))
    }

    fn from_valid_namespace(namespace: String, identity_path: &Path) -> WorkspaceKey {
        let path = identity_path.as_os_str().as_encoded_bytes();
        let mut bytes = Vec::with_capacity(namespace.len() + path.len() + 1);
        bytes.extend_from_slice(namespace.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(path);
        WorkspaceKey {
            namespace,
            id: crate::hashid::fnv1a128_hex(&bytes),
        }
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn state_key(&self, destination: &str) -> WorkspaceStateKey {
        WorkspaceStateKey {
            workspace_id: self.id.clone(),
            destination_id: crate::hashid::fnv1a128_hex(destination.as_bytes()),
        }
    }

    fn remote_namespace_root(&self) -> String {
        format!(".ulak/workspaces/{}", self.namespace())
    }

    fn remote_workspace_root(&self) -> String {
        format!("{}/{}", self.remote_namespace_root(), self.id())
    }
}

/// Local receipts for one transported workspace on one SSH destination.
///
/// The remote locator deliberately remains destination-independent: two
/// servers have separate filesystems already. Local deletion ownership does
/// not. A receipt from one server must never retire a claim on another, so
/// every sync fact uses this composite key while the checkout-wide lock and
/// manifest UUID continue to use `WorkspaceKey` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceStateKey {
    workspace_id: String,
    destination_id: String,
}

impl WorkspaceStateKey {
    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub(crate) fn destination_id(&self) -> &str {
        &self.destination_id
    }
}

fn namespace_is_valid(raw: &str) -> bool {
    let mut chars = raw.chars();
    raw.len() <= 48
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn configured_namespace(raw: &str) -> Result<String> {
    if namespace_is_valid(raw) {
        return Ok(raw.to_string());
    }
    Err(fail!("workspace namespace {raw:?} is invalid")
        .now(
            "use 1-48 lowercase ASCII letters, digits, '-' or '_', starting with a letter or digit",
        )
        .now("set it under [workspace] as: namespace = \"my-laptop\"")
        .into_err())
}

fn automatic_namespace() -> Result<String> {
    automatic_namespace_in(&crate::invocation::state_dir_required()?)
}

/// Resolve the automatic namespace below an explicit root so unit tests do
/// not race process-wide HOME or XDG_STATE_HOME variables.
fn automatic_namespace_in(state_root: &Path) -> Result<String> {
    let dir = state_root.join("client");
    crate::invocation::private_dir(&dir).map_err(|e| {
        fail!(
            "cannot create the private Ulak client state at {}: {e}",
            dir.display()
        )
        .now("check that the local state directory is writable, then retry")
        .into_err()
    })?;

    // Separate Ulak processes can discover a fresh state root together. The
    // OS lock makes mint + atomic rename one decision, so both remember the
    // same namespace instead of racing two valid answers into the file.
    let lock_path = dir.join("namespace.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| {
            fail!(
                "cannot open the workspace namespace lock {}: {e}",
                lock_path.display()
            )
            .now("check that the local state directory is writable, then retry")
            .into_err()
        })?;
    lock.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| {
            fail!(
                "cannot make the workspace namespace lock private at {}: {e}",
                lock_path.display()
            )
            .now("check that the local state directory is writable, then retry")
            .into_err()
        })?;
    lock.lock().map_err(|e| {
        fail!(
            "cannot lock the workspace namespace at {}: {e}",
            lock_path.display()
        )
        .now("check the local filesystem and retry")
        .into_err()
    })?;

    let path = dir.join("namespace");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let raw = String::from_utf8(bytes).map_err(|_| {
                fail!(
                    "the automatic workspace namespace at {} is not UTF-8",
                    path.display()
                )
                .now(
                    "restore its previous value, or configure the intended value under [workspace]",
                )
                .into_err()
            })?;
            if namespace_is_valid(&raw) {
                return Ok(raw);
            }
            return Err(fail!(
                "the automatic workspace namespace at {} is invalid",
                path.display()
            )
            .now("restore its previous value, or configure the intended value under [workspace]")
            .now("do not silently replace it: that would leave existing remote workspaces behind")
            .into_err());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(fail!(
                "cannot read the automatic workspace namespace at {}: {e}",
                path.display()
            )
            .now("check that the local state directory is readable, then retry")
            .into_err());
        }
    }

    let uuid = crate::hashid::uuid_v4().map_err(|e| {
        fail!("cannot create an automatic workspace namespace: {e}")
            .now("check that /dev/urandom is available, then retry")
            .into_err()
    })?;
    let namespace = format!("client-{}", uuid.replace('-', ""));
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    crate::invocation::write_private(&tmp, namespace.as_bytes()).map_err(|e| {
        fail!(
            "cannot write the automatic workspace namespace at {}: {e}",
            tmp.display()
        )
        .now("check that the local state directory is writable, then retry")
        .into_err()
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail!(
            "cannot install the automatic workspace namespace at {}: {e}",
            path.display()
        )
        .now("check that the local state directory is writable, then retry")
        .into_err()
    })?;
    Ok(namespace)
}

/// A configured local working area, independent of Compose.
///
/// The nearest ulak config is the boundary.  It is enough to answer
/// which server Docker daemon commands target; Compose adds its
/// invocation only when the `docker compose` branch is entered.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub name: String,
    pub config: Config,
    pub(crate) workspace_key: WorkspaceKey,
}

impl Workspace {
    pub fn locate() -> Result<Workspace> {
        let cwd = std::env::current_dir()
            .context("cannot read current directory")?
            .canonicalize()
            .context("cannot canonicalize current directory")?;
        let root = workspace_home(&cwd).ok_or_else(|| {
            fail!(
                "no Ulak workspace in {} or any parent directory",
                cwd.display()
            )
            .now("run: ulak init <ssh-host>")
            .into_err()
        })?;
        Self::load(cwd, root, global_config_path().as_deref())
    }

    pub fn for_init() -> Result<Workspace> {
        let cwd = std::env::current_dir()
            .context("cannot read current directory")?
            .canonicalize()
            .context("cannot canonicalize current directory")?;
        let root = workspace_home(&cwd).unwrap_or_else(|| cwd.clone());
        Self::load(cwd, root, global_config_path().as_deref())
    }

    fn load(cwd: PathBuf, root: PathBuf, global: Option<&Path>) -> Result<Workspace> {
        let config = load_layers(&root, global)?;
        let workspace_key = WorkspaceKey::resolve(&config, &root)?;
        let name = sanitize_name(
            root.file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
                .as_ref(),
        );
        let workspace = Workspace {
            cwd,
            root,
            name,
            config,
            workspace_key,
        };
        crate::audit::set_context(workspace.workspace_id());
        Ok(workspace)
    }

    pub fn ssh_dest(&self) -> Result<String> {
        ssh_dest(&self.config)
    }

    pub fn workspace_id(&self) -> &str {
        self.workspace_key.id()
    }

    pub fn remote_workspace_root(&self) -> String {
        self.workspace_key.remote_workspace_root()
    }

    /// A `docker build` / `docker run` context, which speaks no Compose
    /// at all — so the identity is the workspace's own name and no
    /// model will ever refine it.
    pub fn into_project(self, anchor: PathBuf) -> Project {
        let inv = Invocation::workspace(self.cwd, self.root.clone());
        Project {
            inv,
            config_home: self.root,
            anchor,
            identity: self.name.clone(),
            stack_pin: None,
            name: self.name,
            config: self.config,
            workspace_key: self.workspace_key,
        }
    }
}

/// The daemon and the Compose project one declared stack already addresses.
///
/// Historical facts recorded when `up` declared the stack, not questions
/// current config or a changed Compose `name:` may answer again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StackPin {
    pub(crate) destination: String,
    pub(crate) identity: String,
}

/// Where one command's Docker work goes, and what that displaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Handed to OpenSSH and rsync: the pin when there is one, else the
    /// config cascade.
    pub dest: String,
    /// The config cascade's own answer, where a pin overrode it.
    pub drift: Option<DestinationDrift>,
}

/// What the merged config answers instead, where a pin displaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationDrift {
    /// The layers now name a different server.
    Configured(String),
    /// The layers name no server at all any more. Not a disagreement:
    /// nothing else claims to know where the stack lives.
    Unconfigured,
}

/// A located project: the canonical invocation + the merged config.
///
/// The prototype called this "the project root" and meant a directory
/// tree. There is no such tree any more — a compose project is the SET
/// of paths its files reference, so what stays here is the invocation
/// that names those files and the config that governs them.
#[derive(Debug, Clone)]
pub struct Project {
    pub inv: Invocation,
    /// Directory whose `ulak*.toml` were merged. Usually the
    /// project directory; for a monorepo it may be an ancestor.
    pub config_home: PathBuf,
    /// Deepest common ancestor of everything that must reach the server.
    /// The remote workspace reproduces layout relative to this, never its contents —
    /// what actually travels is the footprint (F2).
    pub anchor: PathBuf,
    /// Sanitized project name (compose-safe: lowercase, [a-z0-9_-]).
    /// Names the remote workspace DIRECTORY, not the compose project —
    /// see `config::docker_project_name` for why those are two answers.
    pub name: String,
    /// The compose project this invocation addresses.
    ///
    /// Set twice, exactly like `anchor` beside it: `load_with_global`
    /// puts docker's rungs 1, 2 and 5 in, which is all argv and the
    /// environment can say, and `bind_located` replaces it with what the
    /// server's own `docker compose config` decided — the same cascade,
    /// including the two rungs that live inside files this crate does
    /// not open. A route that never reaches a server keeps the first
    /// answer, and it is the same rung computed the same way.
    pub identity: String,
    /// The daemon and Docker project a declared stack already addresses.
    /// `None` is the ordinary CLI path, where current inputs answer both.
    pub(crate) stack_pin: Option<StackPin>,
    pub config: Config,
    pub(crate) workspace_key: WorkspaceKey,
}

impl Project {
    /// Capture the invocation, then load and merge all three config
    /// layers. `argv` is compose-style (globals first); ulak's own
    /// commands pass the globals typed before the command, which is
    /// often empty.
    pub fn locate(argv: &[String]) -> Result<Project> {
        let inv = Invocation::capture(argv)?;
        let project = Self::load_with_global(inv, global_config_path().as_deref())?;
        // Ordinary explicit/discovered project selection binds its audit
        // trail here. Management recovery must validate and choose among
        // declarations first, then activates its chosen project itself.
        crate::audit::set_context(project.workspace_id());
        Ok(project)
    }

    /// The same project, from an invocation the caller already holds.
    ///
    /// The service is not standing in anyone's directory — it rebuilds the
    /// invocation from `desired.json` — and management selection may need to
    /// inspect a discovered answer before accepting it. Neither can go
    /// through `locate`, which captures and activates immediately. Audit
    /// ownership stays with the caller: the service keeps its fleet-wide
    /// trail; management activates only after its boundary check.
    pub fn locate_from(inv: Invocation) -> Result<Project> {
        Self::load_with_global(inv, global_config_path().as_deref())
    }

    /// Rebuild a lifecycle consumer with the workspace key recorded when its
    /// stack was declared. Current config still supplies sync policy, but a
    /// namespace edit must not move the bytes under a declared stack.
    pub(crate) fn locate_from_pinned(
        inv: Invocation,
        workspace_key: WorkspaceKey,
    ) -> Result<Project> {
        Self::load_with_global_and_key(inv, global_config_path().as_deref(), Some(workspace_key))
    }

    fn load_with_global(inv: Invocation, global: Option<&Path>) -> Result<Project> {
        Self::load_with_global_and_key(inv, global, None)
    }

    fn load_with_global_and_key(
        inv: Invocation,
        global: Option<&Path>,
        pinned: Option<WorkspaceKey>,
    ) -> Result<Project> {
        let config_home = find_config_home(&inv.project_dir);
        let config = load_layers(&config_home, global)?;
        let workspace_key = match pinned {
            Some(key) => key,
            None => WorkspaceKey::resolve(&config, inv.workspace_identity_path())?,
        };
        // The remote DIRECTORY segment, which is not the compose
        // project name and must not become it: this one may be spelled
        // however ulak likes, and changing it would move every existing
        // workspace. `docker_project_name` owns the other question.
        let name = sanitize_name(
            inv.project_dir
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
                .as_ref(),
        );
        let anchor = inv.base_anchor();
        let identity = inv.compose_identity();
        Ok(Project {
            inv,
            config_home,
            anchor,
            name,
            identity,
            stack_pin: None,
            config,
            workspace_key,
        })
    }

    /// Path of `p` inside the workspace. Every local path ulak names on
    /// the server goes through here, so "remote minus workspace == local
    /// minus anchor" holds by construction.
    pub fn remote_rel(&self, p: &Path) -> Result<String> {
        crate::invocation::anchor_rel(&self.anchor, p).ok_or_else(|| {
            fail!(
                "{} sits outside the workspace anchor {}",
                p.display(),
                self.anchor.display()
            )
            .now("report this: it means ulak computed an anchor that does not cover the project")
            .into_err()
        })
    }

    /// The SSH destination for this project, ready for OpenSSH.
    pub fn ssh_dest(&self) -> Result<String> {
        Ok(self.destination()?.dest)
    }

    /// Where this command's Docker work goes, and what that displaced.
    ///
    /// The ONE place the pin and the config cascade meet. A caller that
    /// re-reads the cascade beside a pinned project has manufactured the
    /// second answer this whole class of bug is made of.
    pub fn destination(&self) -> Result<Destination> {
        let Some(pin) = &self.stack_pin else {
            return Ok(Destination {
                dest: ssh_dest(&self.config)?,
                drift: None,
            });
        };
        // Validated already, by `Desired::rebuild_for_stack`. Sending it
        // through `validate_ssh_dest` again would blame a TOML layer the
        // value never came from, and offer to edit it.
        Ok(Destination {
            dest: pin.destination.clone(),
            drift: drift_of(&self.config, &pin.destination)?,
        })
    }

    /// Remote workspace directory, relative to the remote home directory.
    /// Layout contract: ~/.ulak/workspaces/<namespace>/<workspace-id>/<project>/
    /// The workspace root holds the footprint laid out relative to its
    /// ANCHOR, so remote relative paths equal local relative paths.
    pub fn remote_dir(&self) -> String {
        format!("{}/{}", self.remote_workspace_root(), self.name)
    }

    pub fn workspace_namespace(&self) -> &str {
        self.workspace_key.namespace()
    }

    pub fn workspace_id(&self) -> &str {
        self.workspace_key.id()
    }

    pub(crate) fn state_key(&self, destination: &str) -> WorkspaceStateKey {
        self.workspace_key.state_key(destination)
    }

    pub fn remote_namespace_root(&self) -> String {
        self.workspace_key.remote_namespace_root()
    }

    pub fn remote_workspace_root(&self) -> String {
        self.workspace_key.remote_workspace_root()
    }

    /// Same path, shown with ~/ for humans.
    pub fn remote_dir_shown(&self) -> String {
        format!("~/{}", self.remote_dir())
    }

    // The bootstrap directory used to be named here, as one fixed path
    // per workspace. It is built in `footprint::resolve` now, one per
    // INVOCATION, because a single shared one let two ulaks empty it
    // under each other — a path is not the sort of thing to hand out
    // from here once it stops being the same for everybody.

    /// The compose project this invocation addresses — docker's own
    /// cascade, settled by `adopt` and read from here by everything that
    /// needs it.
    pub fn compose_identity(&self) -> String {
        self.identity.clone()
    }

    /// The only project-name override model resolution may add.
    ///
    /// A foreground invocation contributes only an explicit user `-p`;
    /// otherwise Compose must read `.env`, top-level `name:` or its directory
    /// fallback itself. A rebuilt lifecycle consumer instead contributes the
    /// identity recorded when `up` declared the stack, because current name
    /// sources may now describe a different future stack.
    pub(crate) fn compose_model_project_name(&self) -> Option<&str> {
        self.stack_pin
            .as_ref()
            .map(|pin| pin.identity.as_str())
            .or(self.inv.project_name.as_deref())
    }

    /// Bind a rebuilt lifecycle consumer to the daemon and Docker project
    /// that the declaring command actually addressed. These are historical
    /// facts, not questions current config or a changed Compose `name:` may
    /// answer again.
    ///
    /// `config.host` is left exactly as the layers merged it. Writing the
    /// pin into it was the bug `destination()` exists for: the displaced
    /// answer was gone, so nothing could report that the two disagreed.
    pub(crate) fn pin_existing_stack(&mut self, destination: &str, identity: &str) {
        self.identity = identity.to_string();
        self.stack_pin = Some(StackPin {
            destination: destination.to_string(),
            identity: identity.to_string(),
        });
    }

    /// Take the two facts a resolved footprint settles: where this
    /// workspace is anchored, and which compose project it is.
    ///
    /// ONE function because it was two lines copied into three places
    /// and the third copy was missing its second: `doctor` refreshed the
    /// anchor and not the identity, so its pre-flight push wrote a
    /// manifest naming the LOCAL fallback while the `ulak sync` after it
    /// verified against the model's name — and the workspace refused
    /// itself with "the workspace directory on the server belongs to a
    /// DIFFERENT project". Measured on a real server by
    /// `e2e_doctor::a_file_doctor_pushed_can_still_be_retired`, on a
    /// fixture whose compose file names itself.
    pub fn adopt(&mut self, fp: &crate::footprint::Footprint) {
        self.anchor = fp.anchor.clone();
        self.identity = crate::compose::project_name(&self.inv, &fp.model_json);
    }

    /// Refresh the footprint of a stack whose Docker identity was
    /// already settled by a user command.
    ///
    /// The service can run long after `up`. If `ulak.toml`, an env file
    /// or top-level Compose `name:` changes meanwhile, recomputing the
    /// identity would make it abandon the stack that is still running
    /// and start reporting on a different one. `Desired` pins that
    /// identity; only the anchor is allowed to follow the current model.
    pub fn adopt_existing_stack(&mut self, fp: &crate::footprint::Footprint) {
        self.anchor = fp.anchor.clone();
    }

    /// Remote path of this workspace's manifest — SIBLING of the project
    /// dir, so rsync --delete can never touch it.
    pub fn remote_manifest_path(&self) -> String {
        format!("{}/manifest.json", self.remote_workspace_root())
    }
}

/// A project bound to its server AND to its resolved footprint.
///
/// Every command that touches the server starts here, so the invocation,
/// the anchor and the footprint can never disagree — the anchor in
/// particular is only known once the footprint is resolved, and a stale
/// one would mean every remote path is at the wrong depth.
pub struct Bound {
    pub project: Project,
    pub ssh: crate::ssh::Ssh,
    pub dest: String,
    pub footprint: crate::footprint::Footprint,
}

pub fn bind(argv: &[String]) -> Result<Bound> {
    bind_located(Project::locate(argv)?)
}

pub(crate) fn is_projectless(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ui::Fail>()
        .is_some_and(|f| f.msg.starts_with(crate::invocation::NO_PROJECT))
}

/// Bind a project whose invocation has already been selected.
///
/// `bind` is the door for an ordinary typed Compose invocation. Root
/// management commands come through here after `management` has recovered
/// and activated a declaration; other non-Compose routes may deliberately
/// leave the audit context untouched. Selection owns that choice, so this
/// lower layer resolves the server model without making it again.
pub(crate) fn bind_located(mut project: Project) -> Result<Bound> {
    let dest = project.ssh_dest()?;
    let ssh = crate::ssh::Ssh::new(&dest)?;
    let footprint = crate::footprint::resolve_cached(&project, &ssh, &dest)?;
    // The server's compose has now read the env files and the YAML, so
    // it can answer the three rungs argv never could.
    project.adopt(&footprint);
    Ok(Bound {
        project,
        ssh,
        dest,
        footprint,
    })
}

/// Where this project's ulak config lives: the nearest ancestor of
/// the project directory that carries one, else the project directory.
///
/// Root DISCOVERY is gone (it was the wrong question); this only
/// answers "whose rules apply", which a monorepo legitimately
/// answers one level up.
pub fn find_config_home(project_dir: &Path) -> PathBuf {
    workspace_home(project_dir).unwrap_or_else(|| project_dir.to_path_buf())
}

/// What the merged config answers beside a declared server, for a caller
/// that holds no `Project` — `ulak config` reports declarations without
/// selecting one. The single comparison; `Project::destination` uses it
/// too, so a report and a route can never grade the same pair differently.
///
/// `Unconfigured` is exactly "no host line": a host that IS there and
/// fails validation is an error, never a drift. Folding it into
/// `Unconfigured` let a pin hide a broken config — bare commands went on
/// addressing the declared server with no warning, while an unpinned
/// command in the same directory would have refused.
pub fn drift_of(config: &Config, pinned: &str) -> Result<Option<DestinationDrift>> {
    let Some(raw) = config.host.as_deref() else {
        return Ok(Some(DestinationDrift::Unconfigured));
    };
    let configured = validate_ssh_dest(raw)?;
    Ok(if configured == pinned {
        None
    } else {
        Some(DestinationDrift::Configured(configured))
    })
}

/// Config files at a location this Ulak never reads.
///
/// The global template Ulak writes once said the project layers lived under
/// `<project>/.config/`; they live at the root, beside the compose file. A
/// leftover under `.config/` is worse than absent: it still looks like the
/// file to edit, and an edit to it changes nothing. Measured on a real
/// checkout — the stale copy named a server that had been destroyed, the
/// live one named its replacement, and the user could not tell which file
/// Ulak had read.
pub fn misplaced_layers(config_home: &Path) -> Vec<PathBuf> {
    [PROJECT_CONFIG, LOCAL_CONFIG]
        .iter()
        .map(|name| config_home.join(".config").join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// The nearest Ulak config is the local workspace boundary. This is a
/// read-only question: unlike `Workspace::locate`, it neither parses the
/// files nor mints an automatic namespace merely to identify the boundary.
pub(crate) fn workspace_home(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .find(|dir| dir.join(PROJECT_CONFIG).is_file() || dir.join(LOCAL_CONFIG).is_file())
        .map(Path::to_path_buf)
}

fn load_layers(config_home: &Path, global: Option<&Path>) -> Result<Config> {
    let resolved = resolve_layers(config_home, global)?;
    // Ordinary commands still refuse a file they cannot parse; only
    // `ulak config` is allowed to report one and carry on, because a
    // diagnostic that dies on the thing it diagnoses is a dead end.
    for layer in &resolved.layers {
        if let LayerState::Broken(why) = &layer.state {
            return Err(anyhow::anyhow!(why.clone()));
        }
    }
    Ok(resolved.config)
}

pub(crate) fn resolve_layers(config_home: &Path, global: Option<&Path>) -> Result<Resolved> {
    resolve_layers_in(config_home, global, &std::env::vars().collect())
}

/// Where a setting's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    Default,
    File(PathBuf),
    Env(&'static str),
}

/// The winning layer per key, recorded by the merge itself.
#[derive(Debug, Default)]
pub(crate) struct Won(BTreeMap<&'static str, Source>);

impl Won {
    fn set(&mut self, key: &'static str, from: &Path) {
        self.0.insert(key, Source::File(from.to_path_buf()));
    }

    fn set_env(&mut self, key: &'static str, var: &'static str) {
        self.0.insert(key, Source::Env(var));
    }

    pub(crate) fn of(&self, key: &str) -> Source {
        self.0.get(key).cloned().unwrap_or(Source::Default)
    }
}

#[derive(Debug)]
pub(crate) enum LayerState {
    Loaded,
    Absent,
    /// Rendered rather than kept as an error so the report can print it
    /// and `load_layers` can still refuse with the same words.
    Broken(String),
}

#[derive(Debug)]
pub(crate) struct Layer {
    pub label: &'static str,
    pub path: PathBuf,
    /// The global slot was relocated by ULAK_CONFIG.
    pub pointed: bool,
    pub state: LayerState,
}

#[derive(Debug)]
pub(crate) struct Resolved {
    pub config: Config,
    pub layers: Vec<Layer>,
    pub won: Won,
    /// Files at the prototype location under `.config/`, which no layer
    /// above read. See `misplaced_layers`.
    pub ignored: Vec<PathBuf>,
}

/// Resolve the cascade against an explicit environment so tests never
/// touch the process-wide one, which they would race under `cargo test`.
/// The production twin above is the only place that reads `std::env`.
pub(crate) fn resolve_layers_in(
    config_home: &Path,
    global: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<Resolved> {
    let mut config = Config::default();
    let mut won = Won::default();
    let mut layers: Vec<Layer> = Vec::new();

    // ULAK_CONFIG does not add a layer, it RELOCATES the global one —
    // the shape every comparable tool uses (KUBECONFIG, GH_CONFIG_DIR,
    // RCLONE_CONFIG name a file, they do not stack one on top). Keeping
    // it a layer of its own would also make it a project marker, and
    // pointing at /etc/ulak.toml would then move the anchor and re-lay
    // out the workspace on the server.
    let pointed = env_value(env, "ULAK_CONFIG");
    let global_path = match pointed {
        Some(raw) => Some(PathBuf::from(raw)),
        None => global.map(Path::to_path_buf),
    };

    let planned = [
        (global_path, "global", true),
        (Some(config_home.join(PROJECT_CONFIG)), "project", false),
        (Some(config_home.join(LOCAL_CONFIG)), "local", false),
    ];
    for (path, label, is_global) in planned {
        let Some(path) = path else { continue };
        let is_pointed = is_global && pointed.is_some();
        // A discovered ulak.toml may be absent; a file named by
        // ULAK_CONFIG was ASKED for, so its absence is a mistake rather
        // than a default, and running on against the wrong server with no
        // sign the file never loaded is the failure worth refusing.
        if is_pointed && !path.exists() {
            layers.push(Layer {
                label,
                path: path.clone(),
                pointed: true,
                state: LayerState::Broken(ui::flatten(
                    &fail!("ULAK_CONFIG names {}, which does not exist", path.display())
                        .now("point ULAK_CONFIG at a readable Ulak TOML file, or unset it")
                        .into_err(),
                )),
            });
            continue;
        }
        let state = match read_layer(&path) {
            Ok(Some(layer)) => {
                config.apply(layer, is_global, &path, &mut won);
                LayerState::Loaded
            }
            Ok(None) => LayerState::Absent,
            Err(e) => LayerState::Broken(ui::flatten(&e)),
        };
        layers.push(Layer {
            label,
            path,
            pointed: is_pointed,
            state,
        });
    }
    apply_env(&mut config, env, &mut won)?;
    Ok(Resolved {
        config,
        layers,
        won,
        ignored: misplaced_layers(config_home),
    })
}

#[cfg(test)]
fn load_layers_in(
    config_home: &Path,
    global: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<Config> {
    let resolved = resolve_layers_in(config_home, global, env)?;
    for layer in &resolved.layers {
        if let LayerState::Broken(why) = &layer.state {
            return Err(anyhow::anyhow!(why.clone()));
        }
    }
    Ok(resolved.config)
}

/// One row per environment variable Ulak reads as configuration.
///
/// This table is the single inventory: `apply_env`, `ulak config` and the
/// docs all read it, and `every_setting_has_an_environment_variable`
/// proves no TOML key ships without one. The names are mechanical —
/// upper-case the key, dots to underscores, prefix `ULAK_` — but the
/// table is what is maintained, not a string-bending derivation, so the
/// answer can be looked up rather than recomputed.
///
/// `ULAK_TEST_*` is deliberately absent: those are test seams, not
/// product, and one prefix rule labels them without an enumerated list.
pub(crate) struct EnvVar {
    pub name: &'static str,
    /// The TOML key it overrides; `None` for an env-only surface.
    pub key: Option<&'static str>,
    /// Its value never reaches any output: `ulak config` shows only that
    /// it is set, in both the human and the JSON form.
    pub secret: bool,
}

/// The prefix every test seam lives under. Product configuration never
/// uses it, so one rule labels the hooks instead of an enumerated list.
pub(crate) const ENV_TEST_PREFIX: &str = "ULAK_TEST_";

pub(crate) const ENV_VARS: &[EnvVar] = &[
    EnvVar {
        name: "ULAK_CONFIG",
        key: None,
        secret: false,
    },
    EnvVar {
        name: "ULAK_GLOBAL_CONFIG_AUTO",
        key: None,
        secret: false,
    },
    EnvVar {
        name: "ULAK_WORKSPACE_UUID",
        key: None,
        secret: false,
    },
    EnvVar {
        name: "ULAK_SSH_KEY",
        key: None,
        secret: true,
    },
    EnvVar {
        name: "ULAK_SSH_KEY_FILE",
        key: None,
        secret: false,
    },
    EnvVar {
        name: "ULAK_KNOWN_HOSTS",
        key: None,
        secret: false,
    },
    EnvVar {
        name: "ULAK_HOST",
        key: Some("host"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_WORKSPACE_NAMESPACE",
        key: Some("workspace.namespace"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_SYNC_EXCLUDE",
        key: Some("sync.exclude"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_SYNC_INCLUDE",
        key: Some("sync.include"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_SYNC_PROTECT",
        key: Some("sync.protect"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_SYNC_MAX_DELETE",
        key: Some("sync.max_delete"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_FORWARD_AUTO",
        key: Some("forward.auto"),
        secret: false,
    },
    EnvVar {
        name: "ULAK_SERVICE_AUTO",
        key: Some("service.auto"),
        secret: false,
    },
];

/// The workspace identity this run declares, if any.
///
/// A workspace on the server carries a random UUID, and a machine that
/// has lost its own copy cannot tell "mine, before the state directory
/// was wiped" from "another machine using the same checkout path". Only a
/// human knows, so Ulak asks — and a run with no terminal refuses rather
/// than guessing, which is right for a laptop and fatal for CI: the state
/// directory is fresh every job, so the FIRST run creates the workspace
/// and the second is refused.
///
/// This is that human answering in advance, in writing, in the pipeline
/// definition. The alternative — persisting XDG_STATE_HOME across jobs —
/// keeps working and buys back deletion reconciliation as well.
pub fn declared_workspace_uuid() -> Result<Option<String>> {
    declared_workspace_uuid_in(&std::env::vars().collect())
}

fn declared_workspace_uuid_in(env: &BTreeMap<String, String>) -> Result<Option<String>> {
    let Some(raw) = env_value(env, "ULAK_WORKSPACE_UUID") else {
        return Ok(None);
    };
    // It is written into the manifest and compared byte for byte, so a
    // stray newline from a `$(...)` would make every later run see a
    // different identity than the one meant.
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(
            fail!("ULAK_WORKSPACE_UUID is not a workspace identity: `{raw}`")
                .now("use one word with no spaces, such as the uuid ulak minted for this workspace")
                .into_err(),
        );
    }
    Ok(Some(raw.to_string()))
}

/// The variable that overrides one TOML key, for errors that must name
/// the way out of a read-only checkout.
pub(crate) fn env_var_for(key: &str) -> Option<&'static str> {
    ENV_VARS.iter().find(|v| v.key == Some(key)).map(|v| v.name)
}

/// An empty variable is an UNSET one, not an empty value. CI writes
/// `ULAK_HOST=${MAYBE}` all the time, and the predictable answer to a
/// blank is "the layer below still wins", not "the host is the empty
/// string". Matches how `XDG_*` is already read in this codebase.
fn env_value<'a>(env: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name).map(String::as_str).filter(|v| !v.is_empty())
}

/// The topmost file-free layer: every individual variable, applied over
/// whatever the files agreed on.
///
/// Walked against `ENV_VARS` rather than a second hand-written list, so
/// the inventory is the thing maintained and cannot drift from what is
/// actually read.
fn apply_env(config: &mut Config, env: &BTreeMap<String, String>, won: &mut Won) -> Result<()> {
    for var in ENV_VARS {
        let Some(raw) = env_value(env, var.name) else {
            continue;
        };
        if let Some(key) = var.key {
            won.set_env(key, var.name);
        }
        match var.name {
            // Read before the layers, in `load_layers_in`: it decides
            // WHICH file the global layer is, so it cannot be applied
            // after that layer has already been merged.
            "ULAK_CONFIG" => {}
            // Env-only of necessity: the switch that decides whether the
            // global config gets WRITTEN cannot live inside the file it
            // would create. `global_config_auto` reads it leniently
            // before any command runs; the refusal has to happen here or a
            // typo would silently keep meaning "yes, create it".
            "ULAK_GLOBAL_CONFIG_AUTO" => {
                env_bool(var.name, raw)?;
            }
            "ULAK_HOST" => config.host = Some(raw.to_string()),
            "ULAK_WORKSPACE_NAMESPACE" => config.workspace.namespace = Some(raw.to_string()),
            "ULAK_SYNC_EXCLUDE" => config.sync.exclude = env_list(raw),
            "ULAK_SYNC_INCLUDE" => config.sync.include = env_list(raw),
            "ULAK_SYNC_PROTECT" => config.sync.protect = env_list(raw),
            "ULAK_SYNC_MAX_DELETE" => config.sync.max_delete = env_budget(var.name, raw)?,
            "ULAK_FORWARD_AUTO" => config.forward.auto = env_bool(var.name, raw)?,
            // No warning, unlike a project TOML carrying [service]:
            // whoever sets an environment variable is the machine's owner
            // or the CI job's, not a checked-in file speaking for someone
            // else's machine.
            "ULAK_SERVICE_AUTO" => config.service.auto = env_bool(var.name, raw)?,
            // Unreachable while `every_row_in_the_table_reaches_a_field`
            // passes: a row with no arm here would be inventory that does
            // nothing, which is exactly what that test refuses.
            _ => {}
        }
    }
    Ok(())
}

/// Newline-separated, because a gitignore pattern may legitimately
/// contain a comma, a space or a colon — every separator the other tools
/// chose. Newline is the one byte it cannot contain, since that is
/// gitignore's own separator. No `\n` escape is accepted either: a
/// backslash is meaningful in gitignore syntax, so `\n` could honestly
/// mean a literal `n`.
fn env_list(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Only `true` and `false`, the spellings TOML already accepts. Adding
/// git's richer vocabulary (`yes`, `on`, `1`) here would mean two
/// answers to "what is a boolean in Ulak", one per surface.
fn env_bool(name: &str, raw: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(fail!("{name} is not a boolean: `{other}`")
            .now(format!("set {name} to true or false"))
            .into_err()),
    }
}

/// The `--max-delete` value, refused in Ulak's own words.
pub fn parse_delete_budget(raw: &str) -> Result<DeleteBudget> {
    raw.parse().map_err(|()| {
        fail!("--max-delete is not a deletion budget: `{raw}`")
            .now("pass a number, for example --max-delete=25")
            .now(format!(
                "or lift the limit entirely: --max-delete={UNLIMITED}"
            ))
            .into_err()
    })
}

fn env_budget(name: &str, raw: &str) -> Result<DeleteBudget> {
    raw.parse().map_err(|()| {
        fail!("{name} is not a deletion budget: `{raw}`")
            .now(format!("set {name} to a number, for example {name}=25"))
            .now(format!("or lift the limit entirely: {name}={UNLIMITED}"))
            .into_err()
    })
}

fn ssh_dest(config: &Config) -> Result<String> {
    let Some(raw_host) = config.host.as_deref() else {
        let mut err = fail!("no server is configured for this workspace")
            .now("set host = \"<ssh-host>\" in ulak.toml, ulak.local.toml, or your global config");
        // A run with no writable checkout — CI, an agent's container —
        // cannot take the advice above, and the variable that replaces it
        // is the whole reason the environment layer exists.
        if let Some(var) = env_var_for("host") {
            err = err.now(format!("or set it for this run: {var}=<ssh-host>"));
        }
        return Err(err
            .now("or create a project config: ulak init <ssh-host>")
            .into_err());
    };
    validate_ssh_dest(raw_host)
}

/// Validate one destination before it can reach either OpenSSH or rsync.
/// Kept at the config boundary so `init` and loaded config accept exactly
/// the same spellings.
pub(crate) fn validate_ssh_dest(raw_host: &str) -> Result<String> {
    let host = raw_host.trim();
    if host.is_empty() {
        return Err(fail!("the configured SSH host is empty")
            .now("set it to an SSH alias, hostname, or user@hostname")
            .now("or remove that host line to inherit an earlier config layer")
            .into_err());
    }
    let (user, target) = host
        .rsplit_once('@')
        .map_or((None, host), |(user, target)| (Some(user), target));
    if host.starts_with('-')
        || target.starts_with('-')
        || target.is_empty()
        || user.is_some_and(|user| user.is_empty() || user.contains('@'))
        || host.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(fail!("the configured SSH host is not a valid destination")
            .now("use one SSH destination: an alias, hostname, or user@hostname")
            .into_err());
    }
    Ok(host.to_string())
}

fn read_layer(path: &Path) -> Result<Option<RawLayer>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let layer: RawLayer = toml::from_str(&text).map_err(|e| {
        fail!("{} is not valid Ulak TOML: {e}", path.display())
            .now(format!("fix the syntax in {}", path.display()))
            .into_err()
    })?;
    Ok(Some(layer))
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            fail!("HOME is not set")
                .now("export HOME to your home directory and retry")
                .into_err()
        })
}

/// The settings that belong to this MACHINE, read from the global
/// layer alone.
///
/// The service is one per machine, not one per project, so no project's
/// committed config gets a say in whether it runs. Read before any
/// project exists, because the answer decides whether the service starts
/// looking for one.
pub fn machine_config() -> Config {
    machine_config_in(&std::env::vars().collect())
}

fn machine_config_in(env: &BTreeMap<String, String>) -> Config {
    let mut config = Config::default();
    let mut won = Won::default();
    // ULAK_CONFIG relocates this layer here too, or the service would
    // read a different global config than every command does.
    let global = match env_value(env, "ULAK_CONFIG") {
        Some(raw) => Some(PathBuf::from(raw)),
        None => global_config_path(),
    };
    if let Some(path) = global
        && let Ok(Some(layer)) = read_layer(&path)
    {
        config.apply(layer, true, &path, &mut won);
    }
    // A bad value cannot refuse anything here — nobody typed a command —
    // so the machine keeps the default it already had. The same variable
    // is refused loudly on the next command anyone runs.
    let _ = apply_env(&mut config, env, &mut won);
    config
}

/// May the first run in a terminal write the global config?
///
/// Lenient for `machine_config_in`'s reason and no other: this is decided
/// before anyone's command runs, so a value that is not a boolean keeps
/// the default instead of refusing a command nobody has typed yet. The
/// same variable is refused out loud by `apply_env` on that same run.
///
/// `ULAK_CONFIG` answers no as well. It RELOCATES the global layer, so
/// the default path is not the file this run reads, and seeding a layer
/// that will never load is worse than leaving the home directory alone.
pub fn global_config_auto() -> bool {
    global_config_auto_in(&std::env::vars().collect())
}

fn global_config_auto_in(env: &BTreeMap<String, String>) -> bool {
    if env_value(env, "ULAK_CONFIG").is_some() {
        return false;
    }
    match env_value(env, "ULAK_GLOBAL_CONFIG_AUTO") {
        Some(raw) => env_bool("ULAK_GLOBAL_CONFIG_AUTO", raw).unwrap_or(true),
        None => true,
    }
}

pub fn global_config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home_dir().ok()?.join(".config"),
    };
    Some(base.join("ulak/config.toml"))
}

/// Compose-safe project name: lowercase, [a-z0-9_-], must not be empty.
/// Compose's OWN normalization of a derived project name, measured
/// against Compose v5.3.1 rather than remembered:
///
/// | directory        | `docker compose config` says |
/// |------------------|------------------------------|
/// | `UPPER`          | `upper`                      |
/// | `Weird_Dir.Name` | `weird_dirname`              |
/// | `my project`     | `myproject`                  |
/// | `a@b#c`          | `abc`                        |
/// | `ÜST.dizin`      | `stdizin`                    |
/// | `-leading`       | `leading`                    |
/// | `_under`         | `under`                      |
/// | `...`            | refuses: "must not be empty" |
///
/// So: lowercase, DELETE everything outside `[a-z0-9_-]`, then drop
/// leading characters that are not a letter or a digit.
///
/// Deliberately not `sanitize_name` below, and the difference is the
/// point: that one substitutes a `-` for each bad character and answers
/// `project` for a name with none left, which is right for the REMOTE
/// DIRECTORY it names — a directory may be spelled however we like, and
/// changing it would move every existing workspace. This one has to
/// agree with docker character for character, because the string it
/// produces is compared against what the server's own compose computed.
pub fn docker_project_name(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .skip_while(|c| !c.is_ascii_alphanumeric())
        .collect()
}

/// Ulak's own normalization for the remote workspace DIRECTORY. See
/// `docker_project_name` for why the two are not one function.
pub fn sanitize_name(raw: &str) -> String {
    let mut out: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "project".into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A project rooted at `dir`, captured the way a bare `ulak sync`
    /// in that directory would capture it.
    fn load_at(dir: &Path, global: Option<&Path>) -> Result<Project> {
        let inv = Invocation::capture_in(dir, &[], BTreeMap::new())?;
        Project::load_with_global(inv, global)
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// A project at `dir` whose own layer names `host`, or names nothing.
    fn hosted_at(dir: &Path, host: Option<&str>) -> Project {
        if let Some(host) = host {
            write(&dir.join(PROJECT_CONFIG), &format!("host = \"{host}\"\n"));
        }
        write(&dir.join("compose.yaml"), "services: {}\n");
        load_at(dir, None).expect("a project with a compose file")
    }

    /// The bug both accessors exist for, reproduced on a real machine: a
    /// user destroyed their server and pointed the checkout at a new one;
    /// `ulak config` then printed the new host while `ulak doctor` in the
    /// same directory timed out against the old one, with nothing saying
    /// the two disagreed. The pin still WINS — a config edit must not move
    /// a running stack — but it no longer erases what it displaced.
    #[test]
    fn a_pinned_stack_still_wins_but_carries_the_host_the_config_now_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut project = hosted_at(&dir, Some("new-server"));
        project.pin_existing_stack("dead-server", "api");

        let destination = project.destination().expect("a pinned destination");
        assert_eq!(
            destination.dest, "dead-server",
            "routing follows the declaration"
        );
        assert_eq!(
            destination.drift,
            Some(DestinationDrift::Configured("new-server".into())),
            "and the displaced host survives, or no report can name it"
        );
        assert_eq!(
            project.ssh_dest().unwrap(),
            "dead-server",
            "ssh_dest is the same one answer, not a second cascade"
        );
        assert_eq!(
            project.config.host.as_deref(),
            Some("new-server"),
            "the merged config is left intact — overwriting it was the bug"
        );
        assert_eq!(
            project.compose_model_project_name(),
            Some("api"),
            "the identity half of the pin still reaches model resolution"
        );
    }

    /// The accepting, ordinary case: pin and config agree and nothing is
    /// reported. A drift that fired on every declared stack would be noise
    /// on every root command.
    #[test]
    fn a_pin_that_agrees_with_the_config_reports_no_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut project = hosted_at(&dir, Some("same-server"));
        project.pin_existing_stack("same-server", "api");

        let destination = project.destination().unwrap();
        assert_eq!(destination.dest, "same-server");
        assert_eq!(destination.drift, None);
    }

    /// A config that stopped naming any host is not a disagreement: nothing
    /// else claims to know where the stack lives, so the pin answers alone
    /// and the drift says so rather than inventing a second server.
    #[test]
    fn a_pin_over_a_config_with_no_host_is_unconfigured_not_a_disagreement() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut project = hosted_at(&dir, None);
        assert!(
            project.destination().is_err(),
            "with no pin and no host there is nothing to address"
        );
        project.pin_existing_stack("dead-server", "api");

        let destination = project.destination().expect("the pin answers alone");
        assert_eq!(destination.dest, "dead-server");
        assert_eq!(destination.drift, Some(DestinationDrift::Unconfigured));
    }

    /// A host that is configured but could never reach OpenSSH is a broken
    /// config, and a pin must not paper over it: the same directory's
    /// unpinned commands refuse, so the pinned ones refuse with the same
    /// words rather than quietly addressing the declared server.
    #[test]
    fn a_pin_does_not_hide_an_invalid_configured_host() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut project = hosted_at(&dir, Some("-oProxyCommand=x"));
        project.pin_existing_stack("dead-server", "api");

        let err = project
            .destination()
            .expect_err("an invalid host is an error, not a drift");
        assert!(
            ui::flatten(&err).contains("not a valid destination"),
            "{}",
            ui::flatten(&err)
        );
        assert!(project.ssh_dest().is_err());
    }

    /// An ordinary command has no pin: `destination` is exactly the config
    /// cascade, and nothing was displaced.
    #[test]
    fn an_unpinned_project_answers_the_configured_host_with_no_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let project = hosted_at(&dir, Some("only-server"));

        let destination = project.destination().unwrap();
        assert_eq!(destination.dest, "only-server");
        assert_eq!(destination.drift, None);
    }

    /// A leftover at the prototype location is named, and only when it is
    /// there: the ordinary checkout must not be told about a directory it
    /// does not have. Both spellings count, because both were written.
    #[test]
    fn layers_under_dot_config_are_reported_as_ignored_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join(LOCAL_CONFIG), "host = \"read-me\"\n");
        let clean = resolve_layers_in(home, None, &env(&[])).unwrap();
        assert!(clean.ignored.is_empty(), "{:?}", clean.ignored);

        write(
            &home.join(".config").join(LOCAL_CONFIG),
            "host = \"stale\"\n",
        );
        write(&home.join(".config").join(PROJECT_CONFIG), "");
        let resolved = resolve_layers_in(home, None, &env(&[])).unwrap();
        assert_eq!(
            resolved.config.host.as_deref(),
            Some("read-me"),
            "the .config copy must not win, or moving it would change behaviour"
        );
        assert_eq!(
            resolved.ignored,
            vec![
                home.join(".config").join(PROJECT_CONFIG),
                home.join(".config").join(LOCAL_CONFIG),
            ]
        );
    }

    /// The regression this guards: a new TOML key shipping with no way to
    /// set it from the environment, which a CI job or an agent cannot fix
    /// because it has no writable checkout. The schema is read out of this
    /// very file, so adding a field to a `Raw*` struct without a row in
    /// `ENV_VARS` fails here rather than being discovered in a pipeline.
    #[test]
    fn every_setting_has_an_environment_variable() {
        let src = include_str!("config.rs");
        let mut want: Vec<String> = Vec::new();
        let mut section: Option<&str> = None;
        for line in src.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("struct Raw") {
                let name = rest.trim_end_matches(" {");
                section = match name {
                    "Layer" => Some(""),
                    "Workspace" => Some("workspace"),
                    "Sync" => Some("sync"),
                    "Forward" => Some("forward"),
                    "Service" => Some("service"),
                    _ => panic!("unknown config struct Raw{name}: teach this test its prefix"),
                };
                continue;
            }
            let Some(prefix) = section else { continue };
            if line == "}" {
                section = None;
                continue;
            }
            let Some((field, _)) = line.split_once(": Option<") else {
                continue;
            };
            // The nested tables appear as fields of RawLayer too; their
            // own struct contributes the leaf names.
            if ["workspace", "sync", "forward", "service"].contains(&field) && prefix.is_empty() {
                continue;
            }
            want.push(if prefix.is_empty() {
                field.to_string()
            } else {
                format!("{prefix}.{field}")
            });
        }
        assert!(
            want.contains(&"sync.max_delete".to_string()) && want.len() >= 8,
            "the schema scrape found {want:?} — the parser above stopped matching the source"
        );
        for key in want {
            // `service.notify` is parsed and deliberately never read, so
            // an environment variable for it would promise something the
            // product does not do.
            if key == "service.notify" {
                assert!(
                    env_var_for(&key).is_none(),
                    "{key} is dead; it must not gain a variable"
                );
                continue;
            }
            assert!(
                env_var_for(&key).is_some(),
                "{key} has no ULAK_* variable: add a row to ENV_VARS"
            );
        }
    }

    /// A row nobody reads is inventory that lies: `ulak config` would
    /// advertise a variable that changes nothing.
    #[test]
    fn every_row_in_the_table_reaches_a_field() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for var in ENV_VARS {
            let Some(key) = var.key else { continue };
            let probe = match key {
                "sync.max_delete" => "7",
                "forward.auto" | "service.auto" => "false",
                _ => "probe",
            };
            let got = load_layers_in(home, None, &env(&[(var.name, probe)])).unwrap();
            let changed = match key {
                "host" => got.host.as_deref() == Some("probe"),
                "workspace.namespace" => got.workspace.namespace.as_deref() == Some("probe"),
                "sync.exclude" => got.sync.exclude == ["probe"],
                "sync.include" => got.sync.include == ["probe"],
                "sync.protect" => got.sync.protect == ["probe"],
                "sync.max_delete" => got.sync.max_delete == DeleteBudget::Limit(7),
                "forward.auto" => !got.forward.auto,
                "service.auto" => !got.service.auto,
                other => panic!("{other} has a row but this test cannot see it take effect"),
            };
            assert!(changed, "{} set {key} to nothing", var.name);
        }
    }

    #[test]
    fn the_environment_wins_over_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let global = tmp.path().join("global.toml");
        write(&global, "host = \"global\"\n");
        write(&home.join(PROJECT_CONFIG), "host = \"project\"\n");
        write(&home.join(LOCAL_CONFIG), "host = \"local\"\n");

        let files = load_layers_in(home, Some(&global), &BTreeMap::new()).unwrap();
        assert_eq!(files.host.as_deref(), Some("local"));

        let with_env = load_layers_in(home, Some(&global), &env(&[("ULAK_HOST", "env")])).unwrap();
        assert_eq!(with_env.host.as_deref(), Some("env"));
    }

    /// CI writes `ULAK_HOST=${MAYBE_UNSET}` constantly. The predictable
    /// answer to the blank that produces is "the layer below still wins",
    /// not "the host is the empty string", which would then be refused as
    /// an invalid destination.
    #[test]
    fn an_empty_variable_is_an_unset_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join(PROJECT_CONFIG), "host = \"project\"\n");
        let got = load_layers_in(home, None, &env(&[("ULAK_HOST", "")])).unwrap();
        assert_eq!(got.host.as_deref(), Some("project"));
    }

    /// A gitignore pattern may legitimately hold a comma, a space or a
    /// colon — the separators every comparable tool picked. Newline is
    /// the one byte it cannot hold.
    #[test]
    fn a_list_variable_is_split_on_newlines_only() {
        let tmp = tempfile::tempdir().unwrap();
        let got = load_layers_in(
            tmp.path(),
            None,
            &env(&[(
                "ULAK_SYNC_EXCLUDE",
                "*.log\n build, with space/ \n\ndata:1\n",
            )]),
        )
        .unwrap();
        assert_eq!(got.sync.exclude, ["*.log", "build, with space/", "data:1"]);
    }

    /// Lists REPLACE, on every layer. The environment does not get a
    /// merging rule of its own — that would be a second answer to "how do
    /// two layers combine", which is how `.env` silently stops travelling.
    #[test]
    fn a_list_variable_replaces_the_file_and_does_not_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(
            &home.join(PROJECT_CONFIG),
            "[sync]\ninclude = [\".env\", \"keep\"]\n",
        );
        let got = load_layers_in(home, None, &env(&[("ULAK_SYNC_INCLUDE", "only")])).unwrap();
        assert_eq!(got.sync.include, ["only"]);
    }

    #[test]
    fn an_invalid_value_is_refused_quoting_what_was_typed() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = load_layers_in(tmp.path(), None, &env(&[("ULAK_FORWARD_AUTO", "yes")]))
            .unwrap_err()
            .to_string();
        assert!(
            bad.contains("ULAK_FORWARD_AUTO") && bad.contains("`yes`"),
            "{bad}"
        );

        let bad = load_layers_in(tmp.path(), None, &env(&[("ULAK_SYNC_MAX_DELETE", "lots")]))
            .unwrap_err()
            .to_string();
        assert!(
            bad.contains("ULAK_SYNC_MAX_DELETE") && bad.contains("`lots`"),
            "{bad}"
        );
    }

    /// Machine-wide by nature: a checked-in `[service]` is ignored with a
    /// warning because a project cannot speak for someone else's machine,
    /// but whoever exports a variable IS that machine's owner.
    #[test]
    fn the_service_switch_is_read_from_the_environment_without_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let got =
            load_layers_in(tmp.path(), None, &env(&[("ULAK_SERVICE_AUTO", "false")])).unwrap();
        assert!(!got.service.auto);
    }

    /// Every other switch test spells the value `false` or a word that is
    /// not a boolean, so nothing proved `true` was read as ON: a slip that
    /// answered `false` to both spellings would leave
    /// `ULAK_FORWARD_AUTO=true` looking obeyed while no port ever came
    /// back, and it is the ON direction a config file has already turned
    /// off that a CI job actually needs.
    #[test]
    fn a_switch_a_file_turned_off_is_turned_back_on_by_its_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let global = home.join("global.toml");
        write(
            &global,
            "[forward]\nauto = false\n[service]\nauto = false\n",
        );

        let files = load_layers_in(home, Some(&global), &BTreeMap::new()).unwrap();
        assert!(
            !files.forward.auto && !files.service.auto,
            "the file must turn both off, or the variable has nothing to overturn"
        );

        let got = load_layers_in(
            home,
            Some(&global),
            &env(&[("ULAK_FORWARD_AUTO", "true"), ("ULAK_SERVICE_AUTO", "true")]),
        )
        .unwrap();
        assert!(got.forward.auto, "ULAK_FORWARD_AUTO=true must forward");
        assert!(
            got.service.auto,
            "ULAK_SERVICE_AUTO=true must run the service"
        );
    }

    /// ULAK_CONFIG RELOCATES the global layer rather than stacking on
    /// top of the files: the project's own config still wins, exactly as
    /// it does over `~/.config/ulak/config.toml`.
    #[test]
    fn ulak_config_replaces_the_global_layer_and_the_project_still_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let global = tmp.path().join("global.toml");
        let pointed = tmp.path().join("pointed.toml");
        write(&global, "host = \"global\"\n[sync]\nmax_delete = 1\n");
        write(&pointed, "host = \"pointed\"\n[sync]\nmax_delete = 9\n");
        write(&home.join(PROJECT_CONFIG), "host = \"project\"\n");

        let got = load_layers_in(
            home,
            Some(&global),
            &env(&[("ULAK_CONFIG", pointed.to_str().unwrap())]),
        )
        .unwrap();
        assert_eq!(
            got.host.as_deref(),
            Some("project"),
            "the project layer still wins"
        );
        assert_eq!(
            got.sync.max_delete,
            DeleteBudget::Limit(9),
            "the pointed file replaced the global one"
        );
    }

    /// A discovered `ulak.toml` may be absent; this one was asked for by
    /// name. Silently ignoring the typo would run against the wrong
    /// server with no sign that the file never loaded.
    #[test]
    fn a_missing_ulak_config_is_refused_rather_than_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.toml");
        let err = load_layers_in(
            tmp.path(),
            None,
            &env(&[("ULAK_CONFIG", missing.to_str().unwrap())]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("ULAK_CONFIG") && err.contains("nope.toml"),
            "{err}"
        );
    }

    /// A read-only checkout cannot take "edit ulak.toml" for an answer.
    #[test]
    fn the_missing_host_refusal_names_the_variable_too() {
        let err = ui::flatten(&ssh_dest(&Config::default()).unwrap_err());
        assert!(err.contains("ULAK_HOST"), "{err}");
    }

    /// The guard may be lifted, but only by NAME. `999999999` reached the
    /// same place while reading like a typo, and nothing downstream could
    /// tell a lifted guard from a fat-fingered one.
    #[test]
    fn an_unlimited_budget_is_spelled_out_on_every_surface() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        write(
            &home.join(PROJECT_CONFIG),
            "[sync]\nmax_delete = \"unlimited\"\n",
        );
        let from_toml = load_layers_in(home, None, &BTreeMap::new()).unwrap();
        assert_eq!(from_toml.sync.max_delete, DeleteBudget::Unlimited);

        let from_env =
            load_layers_in(home, None, &env(&[("ULAK_SYNC_MAX_DELETE", "unlimited")])).unwrap();
        assert_eq!(from_env.sync.max_delete, DeleteBudget::Unlimited);

        assert_eq!(
            parse_delete_budget("unlimited").unwrap(),
            DeleteBudget::Unlimited
        );
        assert_eq!(parse_delete_budget("25").unwrap(), DeleteBudget::Limit(25));

        // And it says so wherever it is shown.
        assert_eq!(DeleteBudget::Unlimited.to_string(), "unlimited");
        assert_eq!(DeleteBudget::Limit(25).to_string(), "25");
    }

    #[test]
    fn an_unlimited_budget_allows_any_count_and_a_limit_stops_at_its_own() {
        assert!(DeleteBudget::Unlimited.allows(1_000_000));
        assert!(DeleteBudget::Limit(25).allows(25));
        assert!(!DeleteBudget::Limit(25).allows(26));
        assert!(!DeleteBudget::Limit(0).allows(1));
    }

    /// A word that is not `unlimited` must not be read as some number.
    /// In TOML it fails while the file is read, so it becomes the same
    /// "is not valid Ulak TOML" refusal as every other type error.
    #[test]
    fn a_budget_that_is_neither_a_number_nor_unlimited_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(
            &home.join(PROJECT_CONFIG),
            "[sync]\nmax_delete = \"lots\"\n",
        );
        let err = load_layers_in(home, None, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid Ulak TOML"), "{err}");

        let err = load_layers_in(
            tmp.path().join("other").as_path(),
            None,
            &env(&[("ULAK_SYNC_MAX_DELETE", "lots")]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("ULAK_SYNC_MAX_DELETE") && err.contains("`lots`"),
            "{err}"
        );

        assert!(parse_delete_budget("lots").is_err());
        assert!(parse_delete_budget("-1").is_err());
    }

    /// `max_delete = -1` reads like "no limit" and is the shape a shell
    /// habit produces. It arrives on the signed arm, where the conversion
    /// it fails is the only thing between it and a budget of eighteen
    /// quintillion deletions, so it must be refused while the file is
    /// read — as the same "is not valid Ulak TOML" every type error gets.
    #[test]
    fn a_negative_budget_is_no_budget_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join(PROJECT_CONFIG), "[sync]\nmax_delete = -1\n");

        let err = load_layers_in(home, None, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid Ulak TOML"), "{err}");
        assert!(err.contains("-1"), "{err}");
    }

    /// The refusal it removes is the one that stops a wiped runner on its
    /// SECOND job: the first created the workspace, and the second has no
    /// local claim to compare against the uuid the server now holds.
    #[test]
    fn a_declared_workspace_identity_is_taken_as_this_machines_own() {
        assert_eq!(
            declared_workspace_uuid_in(&env(&[("ULAK_WORKSPACE_UUID", "9f2c-fixed")])).unwrap(),
            Some("9f2c-fixed".to_string())
        );
        assert_eq!(
            declared_workspace_uuid_in(&BTreeMap::new()).unwrap(),
            None,
            "unset must stay unset: the laptop still gets asked"
        );
        assert_eq!(
            declared_workspace_uuid_in(&env(&[("ULAK_WORKSPACE_UUID", "")])).unwrap(),
            None
        );
    }

    /// It is written into the manifest and compared byte for byte, so a
    /// newline picked up from `$(cat id)` would make every later run see
    /// a different identity than the one that was meant.
    #[test]
    fn a_workspace_identity_with_whitespace_is_refused() {
        for bad in ["9f2c fixed", "9f2c\n", "\t9f2c"] {
            let err = declared_workspace_uuid_in(&env(&[("ULAK_WORKSPACE_UUID", bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("ULAK_WORKSPACE_UUID"), "{err}");
        }
    }

    /// The name is not free-form. Every variable is the key upper-cased
    /// with dots turned into underscores, so a setting added later has
    /// one obvious spelling and nobody has to invent — or look up — a
    /// second one.
    #[test]
    fn each_variable_is_named_by_the_mechanical_rule() {
        for var in ENV_VARS {
            let Some(key) = var.key else { continue };
            let expected = format!("ULAK_{}", key.to_uppercase().replace('.', "_"));
            assert_eq!(var.name, expected, "{key} broke the naming rule");
        }
    }

    /// The service decides whether to run at all from this, and it starts
    /// with no project in sight — so it reads the environment the same
    /// way every command does, or `ULAK_SERVICE_AUTO=false` would be
    /// obeyed by the CLI and ignored by the thing it is about.
    #[test]
    fn the_machine_switch_reads_the_environment_like_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let pointed = tmp.path().join("pointed.toml");
        write(&pointed, "[service]\nauto = true\n");

        let on = machine_config_in(&env(&[("ULAK_CONFIG", pointed.to_str().unwrap())]));
        assert!(on.service.auto, "the relocated global layer must be read");

        let off = machine_config_in(&env(&[
            ("ULAK_CONFIG", pointed.to_str().unwrap()),
            ("ULAK_SERVICE_AUTO", "false"),
        ]));
        assert!(!off.service.auto, "the variable must win over that file");
    }

    /// Nobody typed a command here, so there is no one to refuse to: the
    /// machine keeps the default it had, and the same bad value is
    /// refused loudly by the next command anyone runs.
    #[test]
    fn a_bad_value_cannot_stop_the_machine_switch_from_answering() {
        let got = machine_config_in(&env(&[("ULAK_SERVICE_AUTO", "yes")]));
        assert!(got.service.auto, "the default must survive a bad value");
        assert!(
            load_layers_in(
                std::path::Path::new("/nonexistent"),
                None,
                &env(&[("ULAK_SERVICE_AUTO", "yes")])
            )
            .is_err(),
            "and the same value must still be refused where someone is watching"
        );
    }

    #[test]
    fn layers_merge_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let global = tmp.path().join("global.toml");
        write(&root.join("compose.yaml"), "services: {}\n");
        write(&global, "[workspace]\nnamespace = \"global\"\n");
        write(
            &root.join(PROJECT_CONFIG),
            "[workspace]\nnamespace = \"team\"\n[sync]\nexclude = [\"node_modules\"]\nmax_delete = 10\n",
        );
        write(
            &root.join(LOCAL_CONFIG),
            "host = \"demo\"\n[workspace]\nnamespace = \"my-laptop\"\n[sync]\nmax_delete = 99\n",
        );

        let p = load_at(&root, Some(&global)).unwrap();
        assert_eq!(p.config.host.as_deref(), Some("demo"));
        // project layer value survives where local layer is silent
        assert_eq!(p.config.sync.exclude, vec!["node_modules"]);
        // local layer overrides project layer
        assert_eq!(p.config.sync.max_delete, DeleteBudget::Limit(99));
        assert_eq!(p.workspace_namespace(), "my-laptop");
        // defaults survive everywhere else
        assert_eq!(p.config.sync.include, vec![".env"]);
        assert!(p.config.forward.auto);
    }

    /// `ulak config` answers "why is it this value?" from what the merge
    /// recorded, so a branch that sets a field without recording its layer
    /// reports `default` beside a file that plainly sets it — and the
    /// reader goes looking for the setting in the wrong place. Two
    /// branches were never proved to record anything.
    #[test]
    fn the_layer_that_set_protect_or_the_forward_switch_is_the_one_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let project = home.join(PROJECT_CONFIG);
        write(
            &project,
            "[sync]\nprotect = [\"db/\"]\n[forward]\nauto = false\n",
        );

        let got = resolve_layers_in(home, None, &BTreeMap::new()).unwrap();
        assert_eq!(got.config.sync.protect, ["db/"]);
        assert!(!got.config.forward.auto);
        assert_eq!(got.won.of("sync.protect"), Source::File(project.clone()));
        assert_eq!(got.won.of("forward.auto"), Source::File(project));
        assert_eq!(
            got.won.of("sync.exclude"),
            Source::Default,
            "a key no layer touched must still say so"
        );
    }

    #[test]
    fn automatic_namespaces_are_stable_per_client_and_distinct_between_clients() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let a1 = automatic_namespace_in(first.path()).unwrap();
        let a2 = automatic_namespace_in(first.path()).unwrap();
        let b = automatic_namespace_in(second.path()).unwrap();

        assert_eq!(a1, a2, "one client must keep the namespace it minted");
        assert_ne!(
            a1, b,
            "independent local state roots are independent clients"
        );
        assert!(a1.starts_with("client-"));
        assert!(namespace_is_valid(&a1));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(first.path().join("client/namespace"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the client namespace is private state");
    }

    #[test]
    fn a_damaged_automatic_namespace_is_never_silently_replaced() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("client");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("namespace"), b"not/a/segment").unwrap();

        let err = automatic_namespace_in(state.path()).unwrap_err();
        assert!(err.to_string().contains("is invalid"));
        assert_eq!(
            std::fs::read(dir.join("namespace")).unwrap(),
            b"not/a/segment",
            "replacing the value would orphan the old remote workspace"
        );
    }

    #[test]
    fn namespace_and_path_are_both_part_of_the_workspace_key() {
        let path = Path::new("/srv/same/compose.yaml");
        let alice = WorkspaceKey::from_namespace("alice", path).unwrap();
        let alice_again = WorkspaceKey::from_namespace("alice", path).unwrap();
        let bob = WorkspaceKey::from_namespace("bob", path).unwrap();
        let elsewhere =
            WorkspaceKey::from_namespace("alice", Path::new("/srv/other/compose.yaml")).unwrap();

        assert_eq!(alice, alice_again);
        assert_ne!(alice.id(), bob.id());
        assert_ne!(alice.id(), elsewhere.id());
        assert_eq!(alice.id().len(), 32, "the full FNV-128 locator is kept");
    }

    /// A remote filesystem is separate without appearing in its locator,
    /// but its local receipts still need a different key. Otherwise a
    /// deletion confirmed by server A retires server B's ownership row.
    #[test]
    fn workspace_state_is_partitioned_by_destination() {
        let workspace =
            WorkspaceKey::from_namespace("alice", Path::new("/srv/same/compose.yaml")).unwrap();
        let a = workspace.state_key("server-a");
        let a_again = workspace.state_key("server-a");
        let b = workspace.state_key("server-b");

        assert_eq!(a, a_again);
        assert_ne!(a.destination_id(), b.destination_id());
        assert_eq!(a.workspace_id(), workspace.id());
    }

    #[test]
    fn namespace_is_a_safe_unambiguous_directory_segment() {
        for valid in ["a", "7", "alice-laptop", "agent_2", "a123"] {
            assert!(configured_namespace(valid).is_ok(), "{valid:?}");
        }
        for invalid in [
            "",
            "Alice",
            "a/B",
            "a b",
            "-leading",
            "_leading",
            "ümlaut",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(configured_namespace(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_workspace_namespace_never_changes_dockers_project_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("api");
        write(&root.join("compose.yaml"), "services: {}\n");
        let global = tmp.path().join("global.toml");
        write(&global, "[workspace]\nnamespace = \"alice\"\n");

        let project = load_at(&root, Some(&global)).unwrap();
        assert_eq!(project.workspace_namespace(), "alice");
        assert_eq!(project.compose_identity(), "api");
    }

    /// A foreground command must leave `.env` and top-level `name:` to
    /// Compose, while a service rebuild must keep addressing the stack that
    /// was declared before either source changed. This accessor is shared by
    /// the bootstrap command and its cache signature so they cannot diverge.
    #[test]
    fn only_a_historical_stack_or_an_explicit_flag_pins_model_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("api");
        write(&root.join("compose.yaml"), "name: current\nservices: {}\n");

        let mut ordinary = load_at(&root, None).unwrap();
        assert_eq!(ordinary.compose_model_project_name(), None);
        ordinary.pin_existing_stack("server", "historical");
        assert_eq!(ordinary.compose_identity(), "historical");
        assert_eq!(
            ordinary.compose_model_project_name(),
            Some("historical"),
            "a rebuilt service job must override the now-current name source"
        );

        let argv = ["-p".to_string(), "typed".to_string()];
        let inv = Invocation::capture_in(&root, &argv, BTreeMap::new()).unwrap();
        let explicit = Project::load_with_global(inv, None).unwrap();
        assert_eq!(explicit.compose_model_project_name(), Some("typed"));
    }

    #[test]
    fn a_live_stack_keeps_the_workspace_namespace_it_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("api");
        write(&root.join("compose.yaml"), "services: {}\n");
        write(
            &root.join(LOCAL_CONFIG),
            "[workspace]\nnamespace = \"new-client\"\n",
        );
        let inv = Invocation::capture_in(&root, &[], BTreeMap::new()).unwrap();
        let pinned =
            WorkspaceKey::from_namespace("old-client", inv.workspace_identity_path()).unwrap();

        let project = Project::load_with_global_and_key(inv, None, Some(pinned)).unwrap();
        assert_eq!(project.workspace_namespace(), "old-client");
    }

    #[test]
    fn config_home_is_inherited_from_an_ancestor() {
        // A monorepo: rules at the top, compose file deep inside. The
        // compose file's own directory stays the project directory.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        write(&repo.join(PROJECT_CONFIG), "[sync]\nmax_delete = 7\n");
        write(&repo.join("stack/compose.yaml"), "services: {}\n");

        let p = load_at(&repo.join("stack"), None).unwrap();
        assert_eq!(p.config_home, repo);
        assert_eq!(p.inv.project_dir, repo.join("stack"));
        assert_eq!(p.name, "stack");
        assert_eq!(p.config.sync.max_delete, DeleteBudget::Limit(7));
    }

    #[test]
    fn without_any_config_the_project_dir_is_the_config_home() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");
        let p = load_at(&root, None).unwrap();
        assert_eq!(p.config_home, root);
    }

    #[test]
    fn a_manual_config_is_a_workspace_without_compose() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("proj");
        write(&root.join(PROJECT_CONFIG), "[sync]\nmax_delete = 7\n");
        write(&root.join(LOCAL_CONFIG), "host = \"buildbox\"\n");

        let workspace = Workspace::load(root.clone(), root.clone(), None).unwrap();

        assert_eq!(workspace.root, root);
        assert_eq!(workspace.config.sync.max_delete, DeleteBudget::Limit(7));
        assert_eq!(workspace.ssh_dest().unwrap(), "buildbox");
        let id = workspace.workspace_id().to_string();
        let project = workspace.clone().into_project(workspace.root.clone());
        assert_eq!(id, project.workspace_id());
    }

    #[test]
    fn the_service_switch_is_read_from_the_global_layer_only() {
        // Measured before this existed: writing exactly the line the
        // docs recommended made `deny_unknown_fields` reject the file
        // and status/doctor/sync/clean all exited 1. The off switch for
        // a background service cannot be the thing that breaks the CLI.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");

        // Default: on.
        assert!(load_at(&root, None).unwrap().config.service.auto);

        // Global layer decides.
        let global = tmp.path().join("global.toml");
        write(&global, "[service]\nauto = false\nnotify = false\n");
        let p = load_at(&root, Some(&global)).unwrap();
        assert!(!p.config.service.auto);
        assert!(!p.config.service.notify);

        // A cloned repo's committed config cannot turn a machine-wide
        // service on (or off) behind the user's back — but it PARSES,
        // which is the whole point.
        write(&root.join(PROJECT_CONFIG), "[service]\nauto = false\n");
        let p = load_at(&root, None).unwrap();
        assert!(
            p.config.service.auto,
            "a project layer must not steer the service"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");
        write(&root.join(PROJECT_CONFIG), "[sink]\nexclude = []\n");
        let err = load_at(&root, None).unwrap_err();
        assert!(err.to_string().contains("not valid Ulak TOML"));
    }

    #[test]
    fn host_uses_the_normal_cascade_and_is_passed_to_ssh_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");
        let global = tmp.path().join("home-config.toml");
        write(&global, "host = \"default-box\"\n");
        write(&root.join(PROJECT_CONFIG), "host = \"team-box\"\n");
        write(&root.join(LOCAL_CONFIG), "host = \"me@dev-box\"\n");

        let p = load_at(&root, Some(&global)).unwrap();
        assert_eq!(p.ssh_dest().unwrap(), "me@dev-box");

        std::fs::remove_file(root.join(LOCAL_CONFIG)).unwrap();
        let p = load_at(&root, Some(&global)).unwrap();
        assert_eq!(p.ssh_dest().unwrap(), "team-box");

        std::fs::remove_file(root.join(PROJECT_CONFIG)).unwrap();
        let p = load_at(&root, Some(&global)).unwrap();
        assert_eq!(p.ssh_dest().unwrap(), "default-box");
    }

    #[test]
    fn an_empty_host_is_not_a_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");
        write(&root.join(PROJECT_CONFIG), "host = \"   \"\n");

        let err = load_at(&root, None).unwrap().ssh_dest().unwrap_err();
        assert!(err.to_string().contains("SSH host is empty"));
    }

    #[test]
    fn a_host_cannot_become_an_ssh_or_rsync_option() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(&root.join("compose.yaml"), "services: {}\n");

        for host in [
            "-oProxyCommand=anything",
            "user@-oProxyCommand=anything",
            "server -o anything",
            "server\nother",
            "user@",
            "@server",
            "one@two@server",
        ] {
            write(
                &root.join(PROJECT_CONFIG),
                &format!("host = {}\n", toml::Value::String(host.to_string())),
            );
            let err = load_at(&root, None).unwrap().ssh_dest().unwrap_err();
            assert!(
                err.to_string().contains("not a valid destination"),
                "{host:?} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("My Project!"), "my-project");
        // …and the compose-project one does NOT, which is the whole
        // reason there are two. Every row measured against Compose
        // v5.3.1 by putting a stack in a directory of that name and
        // reading `docker compose config --format json | .name`.
        for (dir, docker_says) in [
            ("UPPER", "upper"),
            ("Weird_Dir.Name", "weird_dirname"),
            ("my project", "myproject"),
            ("a@b#c", "abc"),
            ("ÜST.dizin", "stdizin"),
            ("-leading", "leading"),
            ("_under", "under"),
            ("123start", "123start"),
            // Docker refuses this one outright ("project name must not
            // be empty"), and letting it refuse in its own words is the
            // faithful thing to do with a name nothing survives.
            ("...", ""),
        ] {
            assert_eq!(docker_project_name(dir), docker_says, "directory {dir:?}");
        }
        // Every non-ASCII character collapses to '-', then the edges are
        // trimmed — the branch a plain-ASCII input never reaches.
        assert_eq!(sanitize_name("Ünïcøde Prøject"), "n-c-de-pr-ject");
        assert_eq!(sanitize_name("..."), "project");
    }

    /// Both answers, not only the refusal: `env_bool` once shipped with
    /// tests for `false` and for garbage, so its `true` arm never ran.
    #[test]
    fn the_global_config_switch_says_yes_by_default_and_takes_both_words() {
        assert!(global_config_auto_in(&BTreeMap::new()));
        assert!(global_config_auto_in(&env(&[(
            "ULAK_GLOBAL_CONFIG_AUTO",
            "true"
        )])));
        assert!(!global_config_auto_in(&env(&[(
            "ULAK_GLOBAL_CONFIG_AUTO",
            "false"
        )])));
    }

    /// `ULAK_CONFIG` relocates the global layer, so the default path is
    /// not the file this run reads. Seeding it would leave a template
    /// nothing loads in a home directory nobody asked ulak to touch.
    #[test]
    fn a_relocated_global_layer_is_never_seeded() {
        assert!(!global_config_auto_in(&env(&[(
            "ULAK_CONFIG",
            "/somewhere/else.toml"
        )])));
    }

    /// The lenient read cannot refuse anything, so the loud refusal has to
    /// come from the ordinary environment pass — otherwise
    /// `ULAK_GLOBAL_CONFIG_AUTO=no` would quietly go on meaning "yes".
    #[test]
    fn a_typo_in_the_global_config_switch_is_refused_by_the_next_command() {
        let tmp = tempfile::tempdir().unwrap();
        let typo = env(&[("ULAK_GLOBAL_CONFIG_AUTO", "no")]);

        let err = load_layers_in(tmp.path(), None, &typo).unwrap_err();

        let said = ui::flatten(&err);
        assert!(said.contains("ULAK_GLOBAL_CONFIG_AUTO"), "{said}");
        assert!(
            global_config_auto_in(&typo),
            "the pre-command read keeps the default instead of refusing"
        );
    }
}
