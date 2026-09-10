//! `docker bake` against a real server: many targets from one file, each
//! with its own context, secret and output.
//!
//! The process under test gets a poisoned `docker` at the front of PATH.
//! The builds can only pass if planning and execution both use the
//! server's Docker/Buildx; any accidental local Docker CLI dependency
//! writes a marker and exits 97.
//!
//! Each test checks the server directly as well as the local tree. The
//! two halves fail for different reasons — "the build read the wrong
//! thing" and "the result never came home" — and a test that cannot tell
//! them apart sends the next reader to the wrong end of the pipe.

mod common;

use common::{TestServer, Workspace};

/// A Ulak invocation for which calling local Docker is a hard failure.
fn bake_cmd(ws: &Workspace, server: &TestServer) -> assert_cmd::Command {
    use std::os::unix::fs::PermissionsExt;

    let bin = ws.project.join(".poison-local-docker");
    std::fs::create_dir_all(&bin).unwrap();
    let docker = bin.join("docker");
    std::fs::write(
        &docker,
        "#!/bin/sh\nprintf called > \"$(dirname \"$0\")/../.local-docker-was-called\"\nexit 97\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&docker).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&docker, permissions).unwrap();

    let inherited = server
        .env
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let mut cmd = ws.ulak(server);
    cmd.env("PATH", format!("{}:{inherited}", bin.display()));
    cmd
}

fn assert_no_local_docker(ws: &Workspace) {
    assert!(
        !ws.project.join(".local-docker-was-called").exists(),
        "Ulak invoked the local Docker CLI"
    );
}

/// Read a file out of the workspace as it exists ON THE SERVER.
///
/// This is what separates "the bake built the right thing" from "the
/// result came home": the first is answerable even when the second fails.
fn remote_read(ws: &Workspace, server: &TestServer, rel: &str) -> String {
    let ids = ws.workspace_ids();
    let id = ids.first().expect("a workspace was registered");
    let root = ws.remote_workspace_root(id);
    let out = server.ssh(&format!("cat {root}/proj/{rel}"));
    assert!(
        out.status.success(),
        "{rel} is not on the server either:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read(ws: &Workspace, rel: &str) -> String {
    std::fs::read_to_string(ws.project.join(rel))
        .unwrap_or_else(|e| panic!("{rel} did not come home: {e}"))
}

/// Images a bake loads into the remote daemon are the test's to remove:
/// the fixture container is disposable, but a run against a real host
/// (`ULAK_TEST_E2E=host:…`) leaves them behind otherwise.
struct RemoteImages<'a> {
    server: &'a TestServer,
    images: Vec<String>,
}

impl Drop for RemoteImages<'_> {
    fn drop(&mut self) {
        for image in &self.images {
            self.server.ssh(&format!(
                "docker image rm -f {image} >/dev/null 2>&1 || true"
            ));
        }
    }
}

/// A registry used only by one flag-matrix scenario. Its anonymous
/// published port avoids colliding with either a user's registry on a
/// real host or another concurrent e2e binary.
struct RemoteRegistry<'a> {
    server: &'a TestServer,
    name: String,
}

impl RemoteRegistry<'_> {
    fn start<'a>(server: &'a TestServer) -> (RemoteRegistry<'a>, String) {
        server.needs_image("registry:2");
        let name = format!("ulak-bake-registry-{}", std::process::id());
        let _ = server.ssh(&format!("docker rm -f {name} >/dev/null 2>&1 || true"));
        let started = server.ssh(&format!(
            "docker run -d --rm --name {name} -p 127.0.0.1::5000 registry:2"
        ));
        assert!(
            started.status.success(),
            "temporary registry did not start:\n{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let published = server.ssh(&format!("docker port {name} 5000/tcp"));
        assert!(
            published.status.success(),
            "temporary registry port was not published:\n{}",
            String::from_utf8_lossy(&published.stderr)
        );
        let binding = String::from_utf8_lossy(&published.stdout);
        let port = binding
            .trim()
            .rsplit_once(':')
            .map(|(_, port)| port)
            .filter(|port| !port.is_empty())
            .unwrap_or_else(|| panic!("unexpected registry port: {binding}"));
        (RemoteRegistry { server, name }, format!("127.0.0.1:{port}"))
    }
}

impl Drop for RemoteRegistry<'_> {
    fn drop(&mut self) {
        let _ = self.server.ssh(&format!(
            "docker rm -f {} >/dev/null 2>&1 || true",
            self.name
        ));
    }
}

/// A Compose-free workspace pointed at the server, which is the shape a
/// bake-only project has: `ulak init` must accept it without a Compose
/// file before any of this can be tested.
fn bake_workspace(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    let init = ws
        .ulak(server)
        .args(["init", &server.alias])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "Compose-free init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    ws
}

/// The fixture project: three targets that between them cover every kind
/// of local path a bake plan can name — a plain context, a named
/// additional context, a secret file, and two local outputs.
fn write_bake_project(ws: &Workspace, image: &str) {
    ws.write("alpha/marker.txt", "ALPHA-CONTEXT\n");
    ws.write("beta/marker.txt", "BETA-CONTEXT\n");
    ws.write("shared/shared.txt", "SHARED-CONTEXT\n");
    ws.write("secrets/token file", "s3cr3t-value\n");

    // Multi-stage on purpose: a secret is only readable during the build
    // and never lands in a layer, so copying what it produced into a
    // scratch stage is what makes "the secret arrived" observable at all.
    ws.write(
        "alpha/Dockerfile",
        "FROM alpine:3.20 AS build\n\
         RUN --mount=type=secret,id=token cp /run/secrets/token /seen.txt\n\
         FROM scratch\n\
         COPY --from=build /seen.txt /seen.txt\n\
         COPY marker.txt /marker.txt\n",
    );
    // `COPY --from=shared` reads the named context, which only resolves
    // if that directory travelled as an input in its own right.
    ws.write(
        "beta/Dockerfile",
        "FROM scratch\n\
         COPY --from=shared shared.txt /shared.txt\n\
         COPY marker.txt /marker.txt\n",
    );
    ws.write(
        "gamma/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );
    ws.write("gamma/marker.txt", "GAMMA-CONTEXT\n");

    ws.write(
        "docker-bake.hcl",
        &format!(
            r#"
group "default" {{ targets = ["alpha", "beta", "gamma"] }}

target "alpha" {{
  context = "alpha"
  secret  = ["id=token,src=secrets/token file"]
  output  = ["type=local,dest=dist/alpha"]
}}

target "beta" {{
  context  = "beta"
  contexts = {{ shared = "shared" }}
  output   = ["type=local,dest=dist/beta"]
}}

# Loads into whichever daemon runs the build, which is how this test
# proves the build happened over there and not here.
target "gamma" {{
  context = "gamma"
  tags    = ["{image}"]
  output  = ["type=docker"]
}}
"#
        ),
    );
}

#[test]
fn listing_targets_uses_the_servers_buildx_without_local_docker() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);
    ws.write(
        "docker-bake.hcl",
        "group \"default\" { targets = [\"api\", \"web\"] }\n\
         target \"api\" { dockerfile-inline = \"FROM scratch\\n\" }\n\
         target \"web\" { dockerfile-inline = \"FROM scratch\\n\" }\n",
    );

    for argv in [
        &["docker", "bake", "--list=targets"][..],
        &["docker", "buildx", "bake", "--list=targets"][..],
        &["docker", "builder", "bake", "--list=targets"][..],
    ] {
        let out = bake_cmd(&ws, &server).args(argv).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "server-side {argv:?} failed:\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for target in ["api", "web", "default"] {
            assert!(
                stdout.contains(target),
                "{target} missing from {argv:?}:\n{stdout}"
            );
        }
    }

    // `--print` is both the implementation's planning primitive and a
    // public Buildx flag. Passing it explicitly must still return the
    // server's JSON rather than recursively consuming or duplicating it.
    let printed = bake_cmd(&ws, &server)
        .args(["docker", "buildx", "bake", "--print"])
        .output()
        .unwrap();
    assert!(
        printed.status.success(),
        "server-side --print failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&printed.stdout),
        String::from_utf8_lossy(&printed.stderr)
    );
    let printed: serde_json::Value =
        serde_json::from_slice(&printed.stdout).expect("buildx --print must remain JSON on stdout");
    assert!(printed.pointer("/target/api").is_some());
    assert!(printed.pointer("/target/web").is_some());
    assert_no_local_docker(&ws);
    ws.forget_on_server(&server);
}

#[test]
fn a_compose_include_is_discovered_during_the_remote_bootstrap() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);
    ws.write(
        "compose.yaml",
        "include:\n  - \"middle file.yaml\"\nservices: {}\n",
    );
    ws.write(
        "middle file.yaml",
        "include:\n  - \"nested/extra file.yaml\"\nservices: {}\n",
    );
    ws.write(
        "nested/extra file.yaml",
        "services:\n  extra:\n    build:\n      context: api\n",
    );
    ws.write("nested/api/marker.txt", "INCLUDED-CONTEXT\n");
    ws.write(
        "nested/api/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );

    let out = bake_cmd(&ws, &server)
        .args([
            "docker",
            "bake",
            "extra",
            "--set",
            "extra.output=type=local,dest=dist/included",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "included Bake definition was not bootstrapped:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&ws, "dist/included/marker.txt"), "INCLUDED-CONTEXT\n");
    assert_no_local_docker(&ws);
    ws.forget_on_server(&server);
}

#[test]
fn a_multi_target_bake_builds_on_the_server_and_brings_its_outputs_home() {
    let Some(server) = TestServer::start() else {
        return;
    };
    // The `alpha` target's first stage is `FROM alpine:3.20` — the only
    // base any of these targets needs, the other two being `FROM scratch`.
    server.needs_image("alpine:3.20");
    let ws = bake_workspace(&server);

    let suffix = std::process::id();
    let image = format!("ulak-bake-gamma-{suffix}:test");
    let _cleanup = RemoteImages {
        server: &server,
        images: vec![image.clone()],
    };
    write_bake_project(&ws, &image);

    let out = bake_cmd(&ws, &server)
        .args([
            "docker",
            "bake",
            "--metadata-file",
            "dist/build-metadata.json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "remote bake failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // First: the build read the right things over there. The secret's
    // value in the exported tree proves the file was carried and mounted,
    // and shared.txt proves the named context travelled as its own input.
    assert_eq!(
        remote_read(&ws, &server, "dist/alpha/seen.txt"),
        "s3cr3t-value\n",
        "the secret never reached the build"
    );
    assert_eq!(
        remote_read(&ws, &server, "dist/beta/shared.txt"),
        "SHARED-CONTEXT\n",
        "the named additional context did not travel as its own input"
    );
    assert!(
        server
            .ssh(&format!("docker image inspect {image} >/dev/null"))
            .status
            .success(),
        "the bake did not build on the remote daemon"
    );
    // The other half of that claim: nothing was built here. The fixture's
    // daemon lives inside its own container, so this image existing on
    // THIS machine would mean the bake never left home.
    assert!(
        !std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "the image was built on the local daemon, so the bake never reached the server"
    );

    // Then: the outputs came home. A local output the user never sees is
    // the same to them as a build that did not run.
    assert_eq!(read(&ws, "dist/alpha/seen.txt"), "s3cr3t-value\n");
    assert_eq!(read(&ws, "dist/alpha/marker.txt"), "ALPHA-CONTEXT\n");
    assert_eq!(read(&ws, "dist/beta/marker.txt"), "BETA-CONTEXT\n");
    assert_eq!(read(&ws, "dist/beta/shared.txt"), "SHARED-CONTEXT\n");
    let local_metadata = read(&ws, "dist/build-metadata.json");
    let parsed_metadata: serde_json::Value = serde_json::from_str(&local_metadata)
        .expect("the metadata file brought home must contain Buildx JSON");
    assert!(
        parsed_metadata
            .as_object()
            .is_some_and(|map| !map.is_empty()),
        "Buildx returned an empty metadata object"
    );
    assert_eq!(
        remote_read(&ws, &server, "dist/build-metadata.json"),
        local_metadata,
        "the metadata brought home differs from the server's result"
    );
    assert_no_local_docker(&ws);

    ws.forget_on_server(&server);
}

#[test]
fn a_named_context_spelled_cwd_travels_and_still_resolves_over_there() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);

    // `cwd://` is Buildx's own way of saying "this one is on the machine
    // the command was typed on". It is the spelling a REMOTE definition
    // has to use, because there a bare relative path belongs to the
    // fetched repository — and a remote definition is a shape ulak
    // already hands to the server. Reading the `://` as "the server
    // fetches this" left the directory at home: the bake then failed
    // over there naming a directory that exists right here, or built
    // from whatever an earlier command happened to leave at that path.
    //
    // Both halves are worth a real server. That the directory travelled
    // is one claim; that the REMOTE buildx still resolves `cwd://`
    // against the mirrored working directory is a different one, and
    // only the far side can answer it.
    ws.write("lib/lib.txt", "CWD-SCHEME-CONTEXT\n");
    ws.write("svc/marker.txt", "SVC-CONTEXT\n");
    ws.write(
        "svc/Dockerfile",
        "FROM scratch\n\
         COPY --from=lib lib.txt /lib.txt\n\
         COPY marker.txt /marker.txt\n",
    );
    ws.write(
        "docker-bake.hcl",
        r#"
target "svc" {
  context  = "svc"
  contexts = { lib = "cwd://lib" }
  output   = ["type=local,dest=dist/svc"]
}
"#,
    );

    let out = bake_cmd(&ws, &server)
        .args(["docker", "bake", "svc"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the cwd:// named context never reached the server:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        remote_read(&ws, &server, "dist/svc/lib.txt"),
        "CWD-SCHEME-CONTEXT\n",
        "the context spelled cwd:// did not travel as an input of its own"
    );
    assert_eq!(read(&ws, "dist/svc/lib.txt"), "CWD-SCHEME-CONTEXT\n");
    assert_no_local_docker(&ws);

    ws.forget_on_server(&server);
}

#[test]
fn a_bake_run_from_a_subdirectory_resolves_against_that_directory() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);

    // The same relative word — "app" — means one thing from the project
    // root and another from `stack/`. Bake resolves it against the working
    // directory, so a bake run from `stack/` has to read `stack/app` on
    // the server too. This is the property that lets the argv travel
    // unchanged; if it were false the remote bake would quietly build the
    // wrong directory, which is why both candidates exist here and only
    // one of them is right.
    ws.write("app/marker.txt", "ROOT-APP-WRONG\n");
    ws.write(
        "app/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );
    ws.write("stack/app/marker.txt", "SUBDIR-APP-RIGHT\n");
    ws.write(
        "stack/app/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );
    ws.write(
        "stack/docker-bake.hcl",
        "target \"app\" {\n  \
           context = \"app\"\n  \
           output  = [\"type=local,dest=dist/out\"]\n\
         }\n",
    );

    let definition = ws.project.join("stack/docker-bake.hcl");
    let mut cmd = bake_cmd(&ws, &server);
    cmd.current_dir(ws.project.join("stack"));
    let out = cmd
        .arg("docker")
        .arg("buildx")
        .arg("bake")
        .arg("-f")
        .arg(&definition)
        .arg("app")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bake from a subdirectory failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        remote_read(&ws, &server, "stack/dist/out/marker.txt"),
        "SUBDIR-APP-RIGHT\n",
        "the bake resolved its context against the wrong directory on the server"
    );
    assert_eq!(read(&ws, "stack/dist/out/marker.txt"), "SUBDIR-APP-RIGHT\n");
    assert!(
        !ws.project.join("dist").exists(),
        "the output landed at the project root, so cwd was lost in transit"
    );
    assert_no_local_docker(&ws);

    ws.forget_on_server(&server);
}

#[test]
fn a_broken_definition_returns_the_servers_buildx_error_without_local_docker() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);
    ws.write("docker-bake.hcl", "target \"broken\" {\n  context =\n");

    let out = bake_cmd(&ws, &server)
        .args(["docker", "builder", "bake", "broken"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a malformed bake unexpectedly passed"
    );
    assert!(
        stderr.contains("Buildx could not resolve this bake definition")
            && stderr.contains("docker-bake.hcl"),
        "the server's Buildx diagnostic was lost:\n{stderr}"
    );
    assert_no_local_docker(&ws);
    ws.forget_on_server(&server);
}

#[test]
fn bake_flags_reach_buildx_and_change_the_real_remote_result() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let help = server.ssh("docker buildx bake --help");
    assert!(
        help.status.success(),
        "the e2e server's Buildx could not show Bake help:\n{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("--var"),
        "the e2e server's Buildx is too old for the mandatory current-Bake flag matrix:\n{help}"
    );
    server.needs_image("alpine:3.20");
    // `--sbom=true` starts Buildx's external scanner. Under
    // `ULAK_TEST_E2E_HUB=block`, an unseeded scanner made this routing test
    // fail at registry-1.docker.io before the pushed result existed.
    server.needs_image("docker/buildkit-syft-scanner:stable-1");
    let ws = bake_workspace(&server);
    let suffix = std::process::id();
    let loaded = format!("ulak-bake-loaded-{suffix}:test");
    let (_registry, registry) = RemoteRegistry::start(&server);
    let base = format!("{registry}/ulak-bake-base-{suffix}:test");
    let pushed = format!("{registry}/ulak-bake-pushed-{suffix}:test");
    let _images = RemoteImages {
        server: &server,
        images: vec![loaded.clone(), base.clone(), pushed.clone()],
    };
    let seeded_base = server.ssh(&format!(
        "docker tag alpine:3.20 {base} && docker push {base} >/dev/null"
    ));
    assert!(
        seeded_base.status.success(),
        "the temporary registry could not be seeded:\n{}",
        String::from_utf8_lossy(&seeded_base.stderr)
    );

    ws.write(
        "app/Dockerfile",
        &format!(
            "FROM {base}\n\
         ARG MESSAGE=WRONG\n\
         ARG EXTRA=WRONG\n\
         RUN printf '%s|%s\\n' \"$MESSAGE\" \"$EXTRA\" > /flags.txt\n"
        ),
    );
    ws.write("plain/marker.txt", "PLAIN-FLAG-BUILD\n");
    ws.write(
        "plain/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );
    ws.write(
        "docker-bake.hcl",
        &format!(
            r#"
variable "MESSAGE" {{ default = "WRONG" }}

target "app" {{
  context = "app"
  args = {{
    MESSAGE = MESSAGE
    EXTRA   = "WRONG"
  }}
  output = ["type=local,dest=dist/flags"]
}}

target "loaded" {{
  context = "plain"
  tags    = ["{loaded}"]
}}

target "pushed" {{
  context = "plain"
  tags    = ["{pushed}"]
}}
"#
        ),
    );

    // Value-carrying and execution-control flags in one build. The file
    // content proves `--var` and `--set`; a successful uncached build
    // through the named remote builder covers the rest without relying
    // on help text or an argv echo.
    let built = bake_cmd(&ws, &server)
        .args([
            "docker",
            "buildx",
            "bake",
            "--allow=fs.read=.",
            "--builder=default",
            "--debug",
            "--no-cache",
            "--progress=plain",
            "--provenance=false",
            "--pull",
            "--sbom=false",
            "--var",
            "MESSAGE=FROM_VAR",
            "--set",
            "app.args.EXTRA=FROM_SET",
            "app",
        ])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "Bake value/control flags failed remotely:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(read(&ws, "dist/flags/flags.txt"), "FROM_VAR|FROM_SET\n");
    assert!(
        String::from_utf8_lossy(&built.stderr).contains('#'),
        "--progress=plain produced no plain BuildKit progress:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    // Both spellings of the check call return Buildx's analysis without
    // building an output. `outline` exercises a non-check `--call`
    // value and must describe the Dockerfile's arguments.
    for call in ["--call=check", "--check", "--call=outline"] {
        let checked = bake_cmd(&ws, &server)
            .args(["docker", "bake", call, "app"])
            .output()
            .unwrap();
        assert!(
            checked.status.success(),
            "{call} failed remotely:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );
    }

    // `--load` must put the result into the SERVER daemon.
    let load = bake_cmd(&ws, &server)
        .args(["docker", "builder", "bake", "--load", "loaded"])
        .output()
        .unwrap();
    assert!(
        load.status.success(),
        "--load failed remotely:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&load.stdout),
        String::from_utf8_lossy(&load.stderr)
    );
    assert!(
        server
            .ssh(&format!("docker image inspect {loaded} >/dev/null"))
            .status
            .success(),
        "--load did not place the image in the remote daemon"
    );

    // `--push` is tested against an isolated registry on the server,
    // and positive provenance is requested on the same successful push.
    let push = bake_cmd(&ws, &server)
        .args([
            "docker",
            "buildx",
            "bake",
            "--push",
            "--provenance=mode=max",
            "--sbom=true",
            "pushed",
        ])
        .output()
        .unwrap();
    assert!(
        push.status.success(),
        "--push/--provenance/--sbom failed remotely:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    let inspect = server.ssh(&format!("docker buildx imagetools inspect {pushed}"));
    assert!(
        inspect.status.success(),
        "the pushed image is absent from the remote registry:\n{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    assert!(
        String::from_utf8_lossy(&inspect.stdout).contains("attestation-manifest"),
        "the pushed index contains no provenance/SBOM attestation:\n{}",
        String::from_utf8_lossy(&inspect.stdout)
    );

    assert_no_local_docker(&ws);
    ws.forget_on_server(&server);
}

#[test]
fn a_bake_path_outside_the_workspace_is_reported_and_not_carried() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = bake_workspace(&server);

    // A local cache directory above the project is an ordinary thing to
    // write in a bake file. It must not be synced and it must not widen
    // the anchor: the anchor decides the remote layout, so reaching out to
    // a temp directory would move the whole remote workspace to the root
    // of the filesystem. Saying so out loud is the contract — silence here
    // would let a build quietly use a cache that is not there.
    let outside = std::env::temp_dir().join(format!("ulak-bake-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    struct Outside(std::path::PathBuf);
    impl Drop for Outside {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _outside = Outside(outside.clone());

    ws.write("app/marker.txt", "APP\n");
    ws.write(
        "app/Dockerfile",
        "FROM scratch\nCOPY marker.txt /marker.txt\n",
    );
    ws.write(
        "docker-bake.hcl",
        &format!(
            "target \"app\" {{\n  \
               context    = \"app\"\n  \
               cache-from = [\"type=local,src={}\"]\n  \
               output     = [\"type=local,dest=dist/out\"]\n\
             }}\n",
            outside.display()
        ),
    );

    let out = bake_cmd(&ws, &server)
        .args(["docker", "bake", "app"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("outside this workspace"),
        "the outside path was carried or swallowed instead of reported:\n{stderr}"
    );
    // Nothing outside the anchor reached the server, and the anchor did
    // not move to swallow it: the workspace still holds the project and
    // only the project.
    let ids = ws.workspace_ids();
    let id = ids.first().expect("a workspace was registered");
    let root = ws.remote_workspace_root(id);
    let listing = server.ssh(&format!("ls {root}/proj"));
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.contains("app") && !listing.contains("ulak-bake-outside"),
        "the remote workspace is not the project alone:\n{listing}"
    );

    // Note for the next reader: Buildx applies its OWN filesystem
    // entitlement check to a bake that reads outside the working
    // directory, and refuses with `--allow=fs.read=…` unless granted.
    // That refusal is Buildx's to make and is not what this test is
    // about, so the bake's own exit status is deliberately not asserted.
    assert_no_local_docker(&ws);

    ws.forget_on_server(&server);
}
