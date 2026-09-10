//! The compose model, ALWAYS resolved on the server.
//!
//! Architecture constant: ulak never parses compose YAML locally.
//! The one true model is `docker compose config --format json` run on
//! the server — the same files and the same resolver the server-side
//! `up` uses. Not in the workspace itself, though: `footprint::resolve`
//! runs it in a per-invocation bootstrap directory that reproduces the
//! ABSOLUTE LOCAL paths, so the paths the model reports are
//! bootstrap-prefixed and get back-mapped to local ones there.
//!
//! This module answers a SECOND question: which transported workspace a
//! Compose stack's containers are actually running from. That answer is
//! Docker's own — the `config_files` and `working_dir` labels it stamps —
//! never the compose file and never the project name, because two
//! checkouts may legitimately share one project name. `WorkspaceUse`
//! owns the verdict, and it is all-or-nothing: `up` and the service both
//! refuse a stack whose containers disagree rather than hand the whole
//! thing to whichever checkout recreated one service last.
//!
//! And a third, off the same labels in the same `docker ps` round:
//! which SERVICES have a running container — `running_services` — so
//! the tunnel report can tell an open port that answers from an open
//! port with nobody behind it. The label vocabulary lives here, in one
//! place, so the probe, `clean` and `up` can never ask docker three
//! differently-spelled questions.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Deserialize;

use crate::config::Project;
use crate::ssh::sh_quote;
use crate::ui::fail;

#[derive(Debug, Default, Deserialize)]
pub struct Model {
    /// The project name COMPOSE decided on, already normalized by it.
    ///
    /// This is docker's whole cascade in one field, and reading it is
    /// how ulak stopped guessing at the middle of it. Three of docker's
    /// five rungs cannot be answered without opening files this crate
    /// deliberately does not open — an env file's
    /// `COMPOSE_PROJECT_NAME`, a top-level `name:` in the YAML (which is
    /// itself interpolated from those env files, and which the LAST
    /// `-f` wins), and the directory basename under compose's own
    /// normalization. All measured on Compose v5.3.1; see
    /// `config::docker_project_name` for the normalization table.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    #[serde(default)]
    pub configs: BTreeMap<String, FileRef>,
    #[serde(default)]
    pub secrets: BTreeMap<String, FileRef>,
    #[serde(default)]
    pub networks: BTreeMap<String, Resource>,
    #[serde(default)]
    pub volumes: BTreeMap<String, Resource>,
}

/// A network or volume as the model reports it. `external: true` means
/// compose will NOT create it — someone else must have, or `up` fails
/// with a message that says nothing about which command to run.
#[derive(Debug, Default, Deserialize)]
pub struct Resource {
    #[serde(default, deserialize_with = "truthy")]
    pub external: bool,
    /// The resolved name compose will look for (already project-prefixed
    /// for non-external ones).
    #[serde(default)]
    pub name: Option<String>,
}

/// `external` has been a bool for years but older files wrote
/// `external: {name: shared}` — treat a NON-EMPTY object as true rather
/// than failing the whole model on a legacy file. An empty
/// `external: {}` reads as false, i.e. compose-managed.
fn truthy<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Object(o) => !o.is_empty(),
        _ => false,
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct Service {
    #[serde(default)]
    pub volumes: Vec<Volume>,
    #[serde(default)]
    pub build: Option<Build>,
    #[serde(default)]
    pub ports: Vec<Port>,
    #[serde(default)]
    pub network_mode: Option<String>,
}

/// Ports as `config --format json` emits them (long syntax).
/// `published` stays a string: compose keeps ranges ("8080-8090") and
/// absent values there. (No `target` field: tunnels end at the
/// server's published port; the container side never matters here.)
#[derive(Debug, Default, Deserialize)]
pub struct Port {
    #[serde(default, deserialize_with = "string_or_number")]
    pub published: Option<String>,
    #[serde(default)]
    pub host_ip: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
}

fn string_or_number<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(de)?;
    Ok(match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct Volume {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct Build {
    #[serde(default)]
    pub context: Option<String>,
    /// As the model spells it — compose resolves the context to an
    /// absolute path but leaves this one relative to it more often than
    /// not, so the caller has to be ready for either.
    #[serde(default)]
    pub dockerfile: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FileRef {
    #[serde(default)]
    pub file: Option<String>,
}

/// The compose command line for RUNS — every command that reaches the
/// server through `passthrough` (up, down, logs, …):
/// `cd <workspace> && COMPOSE_X=… docker compose -p <id> -f … --profile …`
///
/// Not the only builder. `footprint::bootstrap_config_cmd` builds the
/// same flag set against the bootstrap layout to resolve the model;
/// `commands.rs` hand-writes `status`'s `-p`-only `ps -a`, and
/// `stack_containers` below writes its failure-path probe. A global added
/// here has to be added to the bootstrap one too, or the model is
/// resolved with different flags than the run.
///
/// The `-p`-only probes are a different case and stay that way on purpose:
/// they ask docker what it holds under a project LABEL, which is a
/// question no compose file takes part in.
///
/// Every path the user named is re-emitted as its workspace-relative twin,
/// so the server's compose resolves exactly what the local one would.
/// `--project-directory` is always explicit: compose would otherwise
/// derive it from the first `-f`, and stating it removes the last place
/// the local and remote models could disagree.
///
/// `--profile` is forwarded verbatim, and the faithfulness has an edge
/// worth knowing before it is filed as a ulak bug: measured against
/// plain `docker compose`, a `down` whose services all sit behind a
/// `profiles:` gate touches nothing, prints nothing and exits 0 unless
/// the profile is named. ulak is faithful there — but a command
/// that reports success and stops nothing is exactly the failure class
/// this product exists to expose.
pub fn remote_prefix(project: &Project) -> Result<String> {
    let mut cmd = format!(
        "cd {} && {}docker compose",
        sh_quote(&project.remote_dir()),
        env_prefix(&project.inv.compose_env)
    );
    let mut flag = |name: &str, value: &str| {
        cmd.push(' ');
        cmd.push_str(name);
        cmd.push(' ');
        cmd.push_str(&sh_quote(value));
    };
    flag("-p", &project.compose_identity());
    for f in &project.inv.compose_files {
        flag("-f", &project.remote_rel(f)?);
    }
    flag(
        "--project-directory",
        &project.remote_rel(&project.inv.project_dir)?,
    );
    for p in &project.inv.profiles {
        flag("--profile", p);
    }
    for e in &project.inv.env_files {
        flag("--env-file", &project.remote_rel(e)?);
    }
    Ok(cmd)
}

const PROJECT_LABEL: &str = "com.docker.compose.project";
const CONFIG_FILES_LABEL: &str = "com.docker.compose.project.config_files";
const WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";
const SERVICE_LABEL: &str = "com.docker.compose.service";
const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";

pub type ContainerLabels = BTreeMap<String, String>;

/// How every container in one Compose project relates to a transported
/// workspace. The all-container answer is shared by lifecycle declarations
/// and the background probe: an `any` match silently assigns a mixed stack
/// to whichever checkout happened to recreate one service last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceUse {
    Absent,
    All,
    Other,
    Mixed,
}

/// The containers this stack has on the server, created or running.
///
/// Discovered by the project LABEL alone, so it needs no compose file:
/// the resolved identity is the whole lookup. Deliberately `-aq` rather
/// than `ps -q` — this asks whether a command left
/// anything behind, and a container that was created and then failed
/// its healthcheck is something `down` still has to remove.
///
/// `commands::clean` asks a wider ownership question from Docker's
/// Compose labels instead: which existing stacks name files inside one
/// sync workspace. Changing either question is not automatically a
/// reason to change the other.
pub fn stack_containers(project: &Project, ssh: &crate::ssh::Ssh) -> Result<Vec<ContainerLabels>> {
    container_labels(ssh, Some(&project.compose_identity()))
}

/// The ownership and liveness labels Ulak reads, asked for BY NAME so
/// one `docker ps` round can answer and Docker itself does the quoting.
///
/// `{{.Labels}}` is a comma-joined display string, and a path may hold a
/// comma, so that form cannot be split back apart — which is why the
/// whole-map `docker inspect` was reached for first. But `.Label "name"`
/// answers for one named label and `json` escapes it, so the ambiguity
/// never arises. Probed on Docker 29.6.2 against a stack whose
/// `config_files` really does list three files:
///
///   docker ps -a --filter label=com.docker.compose.project \
///     --format '{"…project":{{json (.Label "…project")}},…}'
///   {"…project":"dev","…config_files":"/a/one.yaml,/a/two.yaml",…}
///
/// The commas inside that value stayed inside one JSON string, which is
/// the whole point.
///
/// ONE round, never `docker ps -q` piped into `docker inspect $ids`: a
/// container can end between the two, and `docker inspect` then exits
/// nonzero while still printing every row it did find. Measured on
/// Docker 29.6.2 with one live id and one absent id — exit 1, the live
/// row on stdout. Read as failure, that made a container ending mid-probe
/// abort `clean` outright and tell the service that EVERY stack on the
/// destination had unreadable labels, closing all its tunnels.
fn label_format() -> String {
    let field = |key: &str| format!("{key:?}:{{{{json (.Label {key:?})}}}}");
    format!(
        "{{{}}}",
        // The last two answer WHICH services are alive behind the
        // stack's tunnels. Asking here costs no extra round — the whole
        // point of the per-label format — and an extra key is inert for
        // every consumer that does not read it (`ContainerLabels` is a
        // map). A container without a label answers `""`.
        [
            PROJECT_LABEL,
            CONFIG_FILES_LABEL,
            WORKING_DIR_LABEL,
            SERVICE_LABEL,
            ONEOFF_LABEL,
        ]
        .map(field)
        .join(",")
    )
}

/// The same labels for RUNNING containers only, as one command the
/// service's batched probe prepends to its own script.
///
/// Deliberately not `-a`: the probe asks what is live now, while `clean`
/// and `up` must also see a created-but-stopped container, which still
/// binds the workspace and can start again without Compose.
pub(crate) fn running_labels_script() -> String {
    format!(
        "docker ps --filter label={PROJECT_LABEL} --format {}",
        sh_quote(&label_format())
    )
}

/// Docker's own Compose ownership records, optionally narrowed to one
/// project. One `docker ps` process answers for every matching
/// container; a large daemon must not pay one process per row.
pub fn container_labels(
    ssh: &crate::ssh::Ssh,
    project: Option<&str>,
) -> Result<Vec<ContainerLabels>> {
    let mut ps = format!("docker ps -a --filter label={PROJECT_LABEL}");
    if let Some(project) = project {
        ps.push_str(" --filter ");
        ps.push_str(&sh_quote(&format!("label={PROJECT_LABEL}={project}")));
    }
    ps.push_str(" --format ");
    ps.push_str(&sh_quote(&label_format()));
    let out = ssh.run_checked(&ps, "reading Docker's Compose ownership labels")?;
    parse_container_labels(&String::from_utf8_lossy(&out.stdout)).map_err(|_| {
        fail!("Docker's Compose labels could not be read, so Ulak left the workspace alone")
            .now(format!(
                "list the containers: ssh {} docker ps -a --filter label={PROJECT_LABEL}",
                ssh.dest
            ))
            .now(format!(
                "then inspect one of them: ssh {} docker inspect CONTAINER_ID",
                ssh.dest
            ))
            .into_err()
    })
}

fn parse_container_labels(text: &str) -> serde_json::Result<Vec<ContainerLabels>> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(serde_json::from_str)
        .collect()
}

/// Whether one Compose container is wholly rooted in this transported
/// workspace. Both labels are Docker's own absolute answers in normal
/// operation and both are required: one matching path cannot vouch for a
/// second missing or contradictory ownership record. Accepting the relative
/// form too keeps the check valid for daemons that report paths relative to
/// the remote home.
///
/// Private on purpose, and deliberately paired with the WIDER
/// `labels_reference_workspace` that `clean` vetoes on: publishing the
/// strict half is how a later caller reaches for the wrong side of a
/// distinction that decides whether files get deleted.
fn labels_use_workspace(labels: &ContainerLabels, workspace_root: &str) -> bool {
    [CONFIG_FILES_LABEL, WORKING_DIR_LABEL].iter().all(|key| {
        labels
            .get(*key)
            .is_some_and(|value| path_uses_workspace(value, workspace_root))
    })
}

fn labels_reference_workspace(labels: &ContainerLabels, workspace_root: &str) -> bool {
    [CONFIG_FILES_LABEL, WORKING_DIR_LABEL]
        .iter()
        .filter_map(|key| labels.get(*key))
        .any(|value| path_uses_workspace(value, workspace_root))
}

fn path_uses_workspace(value: &str, workspace_root: &str) -> bool {
    let relative = format!("{}/", workspace_root.trim_end_matches('/'));
    let absolute = format!("/{relative}");
    value.starts_with(&relative) || value.contains(&absolute)
}

pub fn workspace_use(labels: &[ContainerLabels], workspace_root: &str) -> WorkspaceUse {
    if labels.is_empty() {
        return WorkspaceUse::Absent;
    }
    let ours = labels
        .iter()
        .filter(|labels| labels_use_workspace(labels, workspace_root))
        .count();
    match ours {
        0 => WorkspaceUse::Other,
        n if n == labels.len() => WorkspaceUse::All,
        _ => WorkspaceUse::Mixed,
    }
}

pub fn container_project(labels: &ContainerLabels) -> Option<&str> {
    labels
        .get(PROJECT_LABEL)
        .map(String::as_str)
        .filter(|name| !name.is_empty())
}

/// The compose services these containers vouch for, read off Docker's
/// own `com.docker.compose.service` label. Its value is the YAML
/// service name verbatim. Measured (Docker 29.4.0, Compose v5.1.2):
///
///   docker ps --filter label=com.docker.compose.project=<p> \
///     --format '{…{{json (.Label "com.docker.compose.service")}}…}'
///   {…"com.docker.compose.service":"web","com.docker.compose.oneoff":"False"}
///
/// A `compose run` container carries the SAME service label with
/// `com.docker.compose.oneoff` set to `"True"` (measured, capitalised),
/// and is excluded here: a one-off `run web sh` is not the published
/// service, and counting it would report a port as answered while the
/// real container is down. Two consequences the caller inherits from
/// feeding this `docker ps` without `-a`: a stopped service simply
/// drops out (the point), and a crash-looping container still lists —
/// measured, `--restart=always` on a failing command shows
/// `restarting` in plain `docker ps` — so it reads as alive, which is
/// honest: Docker is still trying it, and flapping the report on every
/// backoff would be noise.
pub fn running_services(labels: &[ContainerLabels]) -> std::collections::BTreeSet<String> {
    labels
        .iter()
        .filter(|c| c.get(ONEOFF_LABEL).map(String::as_str) != Some("True"))
        .filter_map(|c| c.get(SERVICE_LABEL))
        .filter(|name| !name.is_empty())
        .cloned()
        .collect()
}

/// Every Compose project with a container that still depends on one
/// transported workspace, sorted and deduplicated for stable guidance.
pub fn projects_using_workspace(labels: &[ContainerLabels], workspace_root: &str) -> Vec<String> {
    let mut projects: Vec<String> = labels
        .iter()
        // Cleaning asks the conservative, wider question: any path into the
        // workspace is enough to veto deletion, even if another label is
        // absent or points elsewhere. Lifecycle ownership above is stricter.
        .filter(|labels| labels_reference_workspace(labels, workspace_root))
        .filter_map(|labels| container_project(labels).map(str::to_string))
        .collect();
    projects.sort();
    projects.dedup();
    projects
}

/// `COMPOSE_X='v' COMPOSE_Y='w' ` — the variables worth carrying, with
/// the path- and identity-bearing ones already filtered out upstream
/// (they would fight the flags built above).
pub fn env_prefix(vars: &BTreeMap<String, String>) -> String {
    let mut parts: Vec<String> = vars
        .iter()
        .map(|(k, v)| format!("{k}={}", sh_quote(v)))
        .collect();
    parts.sort();
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" ") + " "
    }
}

/// WHICH PROJECT this invocation addresses — docker's full cascade,
/// answered by docker.
///
/// Measured on Compose v5.3.1, in this order:
///
///   1. `-p`
///   2. `COMPOSE_PROJECT_NAME` in the environment
///   3. `COMPOSE_PROJECT_NAME` in `--env-file`, else in `.env`
///   4. a top-level `name:` (interpolated, last `-f` wins)
///   5. the project directory's basename, normalized
///
/// Ulak captures 1 and 2 off argv and the environment, which is where
/// they live. It answers 3, 4 and 5 by READING THEM OFF THE MODEL the
/// server just resolved — the same files, the same resolver, the same
/// answer the server-side `up` will use. That is this module's founding
/// rule applied to the identity as well as to the paths: nothing about
/// a compose file is decided by this crate reading it.
///
/// The fallback is rung 5 computed locally, for the routes that never
/// resolve a model (`docker run`, `docker build`) and for a model that
/// somehow carries no name. It is the same rung, computed the same way.
pub fn project_name(inv: &crate::invocation::Invocation, model_json: &str) -> String {
    if let Some(explicit) = &inv.project_name {
        return explicit.clone();
    }
    parse_model(model_json)
        .ok()
        .and_then(|m| m.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| inv.compose_identity())
}

pub fn parse_model(json: &str) -> Result<Model> {
    serde_json::from_str(json).map_err(|e| {
        fail!("the server returned a compose model Ulak cannot read: {e}")
            .now("check the raw output: ulak docker compose config --format json")
            .into_err()
    })
}

/// One local file reference from the model, in doctor's vocabulary.
#[derive(Debug, PartialEq)]
pub struct LocalRef {
    pub service: String,
    pub kind: RefKind,
    pub source: String,
    pub writable: bool,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum RefKind {
    Volume,
    Config,
    Secret,
    Build,
}

/// One `external: true` network or volume, with the name compose will
/// actually look up.
#[derive(Debug, PartialEq)]
pub struct External {
    pub kind: &'static str,
    pub declared: String,
    pub name: String,
}

/// Networks and volumes compose expects to ALREADY exist. ulak does
/// not create them — docker does not either, and the scope guard says we
/// do not out-invent docker — but doctor has to say they are missing,
/// because compose's own failure names neither the resource nor the fix.
pub fn externals(model: &Model) -> Vec<External> {
    let mut out = Vec::new();
    for (kind, map) in [("network", &model.networks), ("volume", &model.volumes)] {
        for (declared, res) in map.iter() {
            if res.external {
                out.push(External {
                    kind,
                    declared: declared.clone(),
                    name: res.name.clone().unwrap_or_else(|| declared.clone()),
                });
            }
        }
    }
    out
}

/// Ports published on EVERY interface. compose's default (`- "8080:80"`,
/// or an explicit `0.0.0.0`) opens the port on the server's public IP —
/// on a VPS that is the whole internet. `forward` tunnels them to
/// localhost, which makes it look like nothing is exposed; it is not.
pub fn public_ports(model: &Model) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, service) in &model.services {
        for port in &service.ports {
            let Some(published) = &port.published else {
                continue;
            };
            let host_ip = port.host_ip.as_deref().unwrap_or("");
            if host_ip.is_empty() || host_ip == "0.0.0.0" || host_ip == "::" {
                out.push((name.clone(), published.clone()));
            }
        }
    }
    out
}

/// Every path-shaped local reference in the model, in stable order.
pub fn local_refs(model: &Model) -> Vec<LocalRef> {
    let mut refs = Vec::new();
    for (name, service) in &model.services {
        for v in &service.volumes {
            if v.kind == "bind"
                && let Some(src) = &v.source
            {
                refs.push(LocalRef {
                    service: name.clone(),
                    kind: RefKind::Volume,
                    source: src.clone(),
                    writable: !v.read_only,
                });
            }
        }
        if let Some(build) = &service.build
            && let Some(ctx) = &build.context
        {
            refs.push(LocalRef {
                service: name.clone(),
                kind: RefKind::Build,
                source: ctx.clone(),
                writable: false,
            });
        }
    }
    for (name, cfg) in &model.configs {
        if let Some(file) = &cfg.file {
            refs.push(LocalRef {
                service: name.clone(),
                kind: RefKind::Config,
                source: file.clone(),
                writable: false,
            });
        }
    }
    for (name, sec) in &model.secrets {
        if let Some(file) = &sec.file {
            refs.push(LocalRef {
                service: name.clone(),
                kind: RefKind::Secret,
                source: file.clone(),
                writable: false,
            });
        }
    }
    refs
}

/// Every build's context together with the Dockerfile it reads.
///
/// `local_refs` already reports the context — this pairing exists for the
/// one thing that needs both: docker keeps the Dockerfile (and the
/// `.dockerignore` beside it) in the build context no matter what the
/// patterns say, and it looks for `<dockerfile>.dockerignore` before the
/// one at the context root. Reported as the model spells them, because
/// resolving a relative dockerfile needs the context, which is the
/// caller's to back-map.
pub fn builds(model: &Model) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for service in model.services.values() {
        if let Some(build) = &service.build
            && let Some(ctx) = &build.context
        {
            // Compose fills this in itself, but an older model that
            // omits it still means the same file.
            let df = build
                .dockerfile
                .clone()
                .unwrap_or_else(|| "Dockerfile".into());
            out.push((ctx.clone(), df));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_report_their_dockerfile_however_it_is_spelled() {
        let m = parse_model(
            r#"{"services": {
                 "a": {"build": {"context": "/m/app", "dockerfile": "docker/Api.Dockerfile"}},
                 "b": {"build": {"context": "/m/app"}},
                 "c": {"volumes": []}
               }}"#,
        )
        .unwrap();
        assert_eq!(
            builds(&m),
            vec![
                ("/m/app".to_string(), "docker/Api.Dockerfile".to_string()),
                ("/m/app".to_string(), "Dockerfile".to_string()),
            ]
        );
    }

    #[test]
    fn model_parses_and_lists_refs() {
        let json = r#"{
          "name": "demo",
          "services": {
            "web": {
              "volumes": [
                {"type": "bind", "source": "/workspace/site", "target": "/usr/share/nginx/html", "read_only": true},
                {"type": "volume", "source": "named", "target": "/data"}
              ],
              "ports": [{"target": 80, "published": "8080", "host_ip": "127.0.0.1", "protocol": "tcp"}]
            },
            "prober": {
              "build": {"context": "/workspace/app"},
              "volumes": [{"type": "bind", "source": "/workspace/data", "target": "/data"}]
            }
          },
          "configs": {"app_conf": {"file": "/workspace/configs/app.conf"}}
        }"#;
        let model = parse_model(json).unwrap();
        let refs = local_refs(&model);
        // named volume skipped; bind mounts, build ctx and configs listed
        assert_eq!(refs.len(), 4);
        assert!(refs.iter().any(|r| r.kind == RefKind::Build));
        let data = refs
            .iter()
            .find(|r| r.source == "/workspace/data")
            .expect("data ref");
        assert!(data.writable, "no read_only flag means writable");
        let site = refs
            .iter()
            .find(|r| r.source == "/workspace/site")
            .expect("site ref");
        assert!(!site.writable);
    }

    #[test]
    fn unreadable_model_gets_guided_error() {
        let err = parse_model("compose barfed: not json").unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn externals_and_public_ports_are_spotted() {
        let m = parse_model(
            r#"{
              "services": {
                "web":  {"ports": [{"published": "8080", "host_ip": "127.0.0.1"}]},
                "open": {"ports": [{"published": "5432"},
                                   {"published": "9000", "host_ip": "0.0.0.0"}]}
              },
              "networks": {
                "default": {"name": "proj_default"},
                "shared":  {"external": true, "name": "team-net"},
                "legacy":  {"external": {"name": "old"}}
              },
              "volumes": {"pgdata": {"external": true, "name": "pgdata"}}
            }"#,
        )
        .unwrap();

        let ext = externals(&m);
        assert_eq!(
            ext.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["legacy", "team-net", "pgdata"],
            "only external:true resources, with their resolved names"
        );
        assert_eq!(ext[0].kind, "network");
        assert_eq!(ext[2].kind, "volume");

        // 127.0.0.1 is safe; a bare port and an explicit 0.0.0.0 are not.
        assert_eq!(
            public_ports(&m),
            vec![
                ("open".to_string(), "5432".to_string()),
                ("open".to_string(), "9000".to_string())
            ]
        );
    }

    #[test]
    fn compose_env_is_quoted() {
        let vars = BTreeMap::from([
            ("COMPOSE_PROFILES".to_string(), "dev,debug".to_string()),
            ("COMPOSE_BAKE".to_string(), "true".to_string()),
        ]);
        assert_eq!(
            env_prefix(&vars),
            "COMPOSE_BAKE=true COMPOSE_PROFILES='dev,debug' "
        );
        assert_eq!(env_prefix(&BTreeMap::new()), "");
    }

    /// Docker Compose itself is the clean and failed-up ledger: its
    /// container labels bind a project name to the config and working
    /// paths it actually used. Different `-p` names over one workspace
    /// must both be found, while an unrelated checkout stays out.
    #[test]
    fn docker_labels_name_every_container_stack_using_a_workspace() {
        let text = concat!(
            "{\"com.docker.compose.project\":\"zeta\",\"com.docker.compose.project.config_files\":\"/home/u/.ulak/workspaces/alice/abc123/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/u/.ulak/workspaces/alice/abc123/proj\"}\n",
            "{\"com.docker.compose.project\":\"alpha\",\"com.docker.compose.project.config_files\":\"/home/u/.ulak/workspaces/alice/abc123/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/u/.ulak/workspaces/alice/abc123/proj\"}\n",
            "{\"com.docker.compose.project\":\"alpha\",\"com.docker.compose.project.config_files\":\"/home/u/.ulak/workspaces/alice/abc123/proj/other.yaml\",\"com.docker.compose.project.working_dir\":\"/home/u/.ulak/workspaces/alice/abc123/proj\"}\n",
            "{\"com.docker.compose.project\":\"elsewhere\",\"com.docker.compose.project.config_files\":\"/home/u/.ulak/workspaces/bob/abc123/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/u/.ulak/workspaces/bob/abc123/proj\"}\n",
        );
        let labels = parse_container_labels(text).unwrap();
        assert_eq!(
            projects_using_workspace(&labels, ".ulak/workspaces/alice/abc123"),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
        assert!(labels_use_workspace(
            &labels[0],
            ".ulak/workspaces/alice/abc123"
        ));
        assert!(!labels_use_workspace(
            &labels[3],
            ".ulak/workspaces/alice/abc123"
        ));
        assert_eq!(
            workspace_use(&labels[..3], ".ulak/workspaces/alice/abc123"),
            WorkspaceUse::All
        );
        assert_eq!(
            workspace_use(&labels[3..], ".ulak/workspaces/alice/abc123"),
            WorkspaceUse::Other
        );
        assert_eq!(
            workspace_use(&labels, ".ulak/workspaces/alice/abc123"),
            WorkspaceUse::Mixed
        );
        assert_eq!(
            workspace_use(&[], ".ulak/workspaces/alice/abc123"),
            WorkspaceUse::Absent
        );
        let contradictory = BTreeMap::from([
            (
                CONFIG_FILES_LABEL.to_string(),
                "/home/u/.ulak/workspaces/alice/abc123/proj/compose.yaml".to_string(),
            ),
            (
                WORKING_DIR_LABEL.to_string(),
                "/home/u/.ulak/workspaces/bob/abc123/proj".to_string(),
            ),
            (PROJECT_LABEL.to_string(), "partial".to_string()),
        ]);
        assert!(labels_reference_workspace(
            &contradictory,
            ".ulak/workspaces/alice/abc123"
        ));
        assert!(
            !labels_use_workspace(&contradictory, ".ulak/workspaces/alice/abc123"),
            "one matching label must veto clean but cannot grant lifecycle ownership"
        );
        assert!(parse_container_labels("not-json\n").is_err());
    }

    /// A `compose run` one-off carries the same service label as the
    /// real container (measured, Compose v5.1.2), so counting it would
    /// report a stopped service's port as answered whenever somebody has
    /// a `run web sh` open. And a daemon that predates the label answers
    /// `""` through `{{json (.Label …)}}`, which must read as "no
    /// answer", never as a service named "".
    #[test]
    fn one_off_containers_do_not_vouch_for_their_service() {
        let text = concat!(
            "{\"com.docker.compose.project\":\"demo\",\"com.docker.compose.service\":\"web\",\"com.docker.compose.oneoff\":\"False\"}\n",
            "{\"com.docker.compose.project\":\"demo\",\"com.docker.compose.service\":\"web\",\"com.docker.compose.oneoff\":\"True\"}\n",
            "{\"com.docker.compose.project\":\"demo\",\"com.docker.compose.service\":\"db\",\"com.docker.compose.oneoff\":\"False\"}\n",
            "{\"com.docker.compose.project\":\"demo\",\"com.docker.compose.service\":\"\",\"com.docker.compose.oneoff\":\"\"}\n",
        );
        let labels = parse_container_labels(text).unwrap();
        let running = running_services(&labels);
        assert_eq!(
            running.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["db", "web"]
        );
        // Only the one-off flavour of web exists: web is NOT running.
        assert_eq!(
            running_services(&labels[1..2]),
            std::collections::BTreeSet::new()
        );
        // The probe's format must actually ask for both labels, or the
        // answers above are questions nobody posed.
        let format = label_format();
        assert!(format.contains(SERVICE_LABEL), "{format}");
        assert!(format.contains(ONEOFF_LABEL), "{format}");
    }
}
