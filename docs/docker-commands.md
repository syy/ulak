# Docker commands through Ulak

<!-- Generated from crates/ulak/src/catalog.rs. A test fails when this file and
     the routing table disagree, so edit the table and regenerate:
     ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk -->

Measured against Docker CLI 29.4.0, Compose 5.1.2 and Buildx 0.33.0.

300 command paths. 262 are carried to your server, of which 68 are Docker's own shorthands for another spelling on this list. 23 are deliberately left on this machine and 15 are mapped but not built yet — each says which, in its own row.

## How a command travels

| Route | What happens |
| --- | --- |
| `DAEMON` | Runs on the server. Nothing is synced: its inputs are already in Docker. |
| `STREAM` | Same, with a stream that outlives the call — a TTY, a follow, a live feed. |
| `COMPOSE` | Compose's own dispatcher: workspace sync, up/down intent, tunnel lifecycle. |
| `BUILD_SYNC` | The build context and Dockerfile are yours, so they travel and the argv is rewritten. |
| `STACK_SYNC` | The Compose model a stack deploys is yours: the files travel and the -c paths are respelled. |
| `BAKE_SYNC` | The server's Buildx resolves the bootstrapped bake definition; every local path in the resulting plan travels. |
| `FILE_BRIDGE` | An argument names a file on THIS machine. The bytes are streamed; the path is never forwarded. |
| `FOOTPRINT_SYNC` | Bind sources, env and label files are yours. The smallest safe set travels; the argv is rewritten. |
| `NAMESPACE` | Not a command at all: a group whose children are. Its own row carries nothing. |
| `LOCAL_ONLY` | Deliberately not carried. The row says what to do instead. |
| `NOT_YET` | Mapped, transport not built. The row says what is missing. |

## One command, two spellings

Docker gives many commands a second name: a root-level shorthand for the ones people type all day (`ps` for `container ls`), and an unprinted long form on nearly every family (`volume list` for `volume ls`). Both spellings are real and both route the same way here — recorded so the map cannot count one command twice, and so a change to one can never miss the other.

| Shorthand | Same command as |
| --- | --- |
| `docker attach` | `docker container attach` |
| `docker commit` | `docker container commit` |
| `docker cp` | `docker container cp` |
| `docker create` | `docker container create` |
| `docker diff` | `docker container diff` |
| `docker events` | `docker system events` |
| `docker exec` | `docker container exec` |
| `docker export` | `docker container export` |
| `docker history` | `docker image history` |
| `docker images` | `docker image ls` |
| `docker import` | `docker image import` |
| `docker info` | `docker system info` |
| `docker kill` | `docker container kill` |
| `docker load` | `docker image load` |
| `docker logs` | `docker container logs` |
| `docker pause` | `docker container pause` |
| `docker port` | `docker container port` |
| `docker ps` | `docker container ls` |
| `docker pull` | `docker image pull` |
| `docker push` | `docker image push` |
| `docker rename` | `docker container rename` |
| `docker restart` | `docker container restart` |
| `docker rm` | `docker container rm` |
| `docker rmi` | `docker image rm` |
| `docker run` | `docker container run` |
| `docker save` | `docker image save` |
| `docker start` | `docker container start` |
| `docker stats` | `docker container stats` |
| `docker stop` | `docker container stop` |
| `docker tag` | `docker image tag` |
| `docker top` | `docker container top` |
| `docker unpause` | `docker container unpause` |
| `docker update` | `docker container update` |
| `docker wait` | `docker container wait` |
| `docker builder b` | `docker builder build` |
| `docker builder debug b` | `docker builder debug build` |
| `docker builder f` | `docker builder bake` |
| `docker buildx b` | `docker buildx build` |
| `docker buildx debug b` | `docker buildx debug build` |
| `docker buildx f` | `docker buildx bake` |
| `docker checkpoint list` | `docker checkpoint ls` |
| `docker checkpoint remove` | `docker checkpoint rm` |
| `docker config list` | `docker config ls` |
| `docker config remove` | `docker config rm` |
| `docker container list` | `docker container ls` |
| `docker container ps` | `docker container ls` |
| `docker container remove` | `docker container rm` |
| `docker context list` | `docker context ls` |
| `docker context remove` | `docker context rm` |
| `docker image list` | `docker image ls` |
| `docker image remove` | `docker image rm` |
| `docker image rmi` | `docker image rm` |
| `docker network list` | `docker network ls` |
| `docker network remove` | `docker network rm` |
| `docker node list` | `docker node ls` |
| `docker node remove` | `docker node rm` |
| `docker plugin list` | `docker plugin ls` |
| `docker plugin remove` | `docker plugin rm` |
| `docker secret list` | `docker secret ls` |
| `docker secret remove` | `docker secret rm` |
| `docker service list` | `docker service ls` |
| `docker service remove` | `docker service rm` |
| `docker stack down` | `docker stack rm` |
| `docker stack list` | `docker stack ls` |
| `docker stack remove` | `docker stack rm` |
| `docker stack up` | `docker stack deploy` |
| `docker volume list` | `docker volume ls` |
| `docker volume remove` | `docker volume rm` |

## Flags that name a file on this machine

These commands run in the daemon and forward cleanly, all but a flag or two each: the client opens those before it dials the daemon at all, so forwarded they read the SERVER's filesystem — quietly, and with the wrong contents. Ulak refuses them and says what to do instead.

| Command | Flag | Instead |
| --- | --- | --- |
| `docker exec` | `--env-file` | pass the variables themselves — `-e NAME=value` travels on the command line, and the file stays where you can still read it |
| `docker builder create` | `--buildkitd-config` / `--config` | point it at a config already on the server, or create the builder there |
| `docker builder imagetools create` | `--file` / `-f` | name the source images on the command line instead — a registry reference means the same thing from either machine |
| `docker builder imagetools create` | `--metadata-file` | drop it: the file would be written on the server, out of reach here — `imagetools inspect` on the new tag afterwards gives you the digest |
| `docker buildx create` | `--buildkitd-config` / `--config` | point it at a config already on the server, or create the builder there |
| `docker buildx imagetools create` | `--file` / `-f` | name the source images on the command line instead — a registry reference means the same thing from either machine |
| `docker buildx imagetools create` | `--metadata-file` | drop it: the file would be written on the server, out of reach here — `imagetools inspect` on the new tag afterwards gives you the digest |
| `docker container exec` | `--env-file` | pass the variables themselves — `-e NAME=value` travels on the command line, and the file stays where you can still read it |
| `docker service create` | `--env-file` | pass the variables themselves — `-e NAME=value` travels on the command line, and the file stays where you can still read it |
| `docker swarm ca` | `--ca-cert` | put the PEM on the server and run it there over ssh — `ulak status` names the host this workspace is bound to |
| `docker swarm ca` | `--ca-key` | put the PEM on the server and run it there over ssh — `ulak status` names the host this workspace is bound to |
| `docker swarm ca` | `--external-ca` (the `cacert=` field) | put the PEM on the server and run it there over ssh — `ulak status` names the host this workspace is bound to |
| `docker swarm init` | `--external-ca` (the `cacert=` field) | put the PEM on the server and run it there over ssh — `ulak status` names the host this workspace is bound to |
| `docker swarm update` | `--external-ca` (the `cacert=` field) | put the PEM on the server and run it there over ssh — `ulak status` names the host this workspace is bound to |

## The tree

### Root commands

| Command | Route | Notes |
| --- | --- | --- |
| `attach` | STREAM | same as `docker container attach` |
| `bake` | BAKE_SYNC | Build from a file |
| `build` | BUILD_SYNC | Build an image from a Dockerfile |
| `commit` | DAEMON | same as `docker container commit` |
| `cp` | FILE_BRIDGE | same as `docker container cp` |
| `create` | FOOTPRINT_SYNC | same as `docker container create` |
| `diff` | DAEMON | same as `docker container diff` |
| `events` | STREAM | same as `docker system events` |
| `exec` | STREAM | same as `docker container exec` |
| `export` | FILE_BRIDGE | same as `docker container export` |
| `history` | DAEMON | same as `docker image history` |
| `images` | DAEMON | same as `docker image ls` |
| `import` | FILE_BRIDGE | same as `docker image import` |
| `info` | DAEMON | same as `docker system info` |
| `inspect` | DAEMON | Return low-level information on Docker objects |
| `kill` | DAEMON | same as `docker container kill` |
| `load` | FILE_BRIDGE | same as `docker image load` |
| `login` | STREAM | Authenticate to a registry |
| `logout` | DAEMON | Log out from a registry |
| `logs` | STREAM | same as `docker container logs` |
| `pause` | DAEMON | same as `docker container pause` |
| `port` | DAEMON | same as `docker container port` |
| `ps` | DAEMON | same as `docker container ls` |
| `pull` | DAEMON | same as `docker image pull` |
| `push` | DAEMON | same as `docker image push` |
| `rename` | DAEMON | same as `docker container rename` |
| `restart` | DAEMON | same as `docker container restart` |
| `rm` | DAEMON | same as `docker container rm` |
| `rmi` | DAEMON | same as `docker image rm` |
| `run` | FOOTPRINT_SYNC | same as `docker container run` |
| `save` | FILE_BRIDGE | same as `docker image save` |
| `search` | DAEMON | Search Docker Hub for images |
| `start` | STREAM | same as `docker container start` |
| `stats` | STREAM | same as `docker container stats` |
| `stop` | DAEMON | same as `docker container stop` |
| `tag` | DAEMON | same as `docker image tag` |
| `top` | DAEMON | same as `docker container top` |
| `unpause` | DAEMON | same as `docker container unpause` |
| `update` | DAEMON | same as `docker container update` |
| `version` | DAEMON | Show the Docker version information |
| `wait` | STREAM | same as `docker container wait` |
| `compose` | COMPOSE | Docker Compose |

### docker builder — Manage builds

| Command | Route | Notes |
| --- | --- | --- |
| `b` | BUILD_SYNC | same as `docker builder build` |
| `bake` | BAKE_SYNC | Build from a file |
| `build` | BUILD_SYNC | Start a build |
| `create` | DAEMON | Create a new builder instance |
| `dap` | NAMESPACE | namespace only |
| `dap attach` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `dap build` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `debug` | NAMESPACE | namespace only |
| `debug b` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `debug build` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `dial-stdio` | STREAM | Proxy current stdio streams to builder instance |
| `du` | DAEMON | Disk usage |
| `f` | BAKE_SYNC | same as `docker builder bake` |
| `history` | NAMESPACE | namespace only |
| `history export` | FILE_BRIDGE | Export build records into Docker Desktop bundle |
| `history import` | LOCAL_ONLY | this reaches into Docker Desktop, which runs on THIS machine and not on the server; run it with plain `docker` here |
| `history inspect` | DAEMON | Inspect a build |
| `history inspect attachment` | DAEMON | Inspect a build record attachment |
| `history logs` | STREAM | Print the logs of a build record |
| `history ls` | DAEMON | List build records |
| `history open` | LOCAL_ONLY | this reaches into Docker Desktop, which runs on THIS machine and not on the server; run it with plain `docker` here |
| `history rm` | DAEMON | Remove build records |
| `history trace` | NOT_YET | trace serves the trace viewer on 127.0.0.1 of whichever machine runs it, so forwarded it binds a port on the server and prints a URL no browser here can open — it wants the tunnel Ulak already builds for Compose, which is not wired to it yet |
| `imagetools` | NAMESPACE | namespace only |
| `imagetools create` | DAEMON | Create a new image based on source images |
| `imagetools inspect` | DAEMON | Show details of an image in the registry |
| `inspect` | DAEMON | Inspect current builder instance |
| `install` | LOCAL_ONLY | install and uninstall rewrite the `docker builder` alias in ~/.docker/config.json on whichever machine runs them, so forwarded they would edit the SERVER's client config and leave this one alone — which is nobody's intent. Buildx 0.33.0 calls both deprecated in any case: type `docker buildx` directly |
| `ls` | DAEMON | List builder instances |
| `policy` | NAMESPACE | namespace only |
| `policy eval` | NOT_YET | `policy eval` takes a local source and reads the `.rego` policy file beside its Dockerfile, and `policy test` takes a local test path — the same footprint `build` already syncs, not yet wired to these two |
| `policy test` | NOT_YET | `policy eval` takes a local source and reads the `.rego` policy file beside its Dockerfile, and `policy test` takes a local test path — the same footprint `build` already syncs, not yet wired to these two |
| `prune` | DAEMON | Remove build cache |
| `rm` | DAEMON | Remove one or more builder instances |
| `stop` | DAEMON | Stop builder instance |
| `uninstall` | LOCAL_ONLY | install and uninstall rewrite the `docker builder` alias in ~/.docker/config.json on whichever machine runs them, so forwarded they would edit the SERVER's client config and leave this one alone — which is nobody's intent. Buildx 0.33.0 calls both deprecated in any case: type `docker buildx` directly |
| `use` | DAEMON | Set the current builder instance |
| `version` | DAEMON | Show buildx version information |

### docker buildx — Docker Buildx

| Command | Route | Notes |
| --- | --- | --- |
| `b` | BUILD_SYNC | same as `docker buildx build` |
| `bake` | BAKE_SYNC | Build from a file |
| `build` | BUILD_SYNC | Start a build |
| `create` | DAEMON | Create a new builder instance |
| `dap` | NAMESPACE | namespace only |
| `dap attach` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `dap build` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `debug` | NAMESPACE | namespace only |
| `debug b` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `debug build` | NOT_YET | the build debugger speaks a local protocol over stdio that Ulak does not bridge yet |
| `dial-stdio` | STREAM | Proxy current stdio streams to builder instance |
| `du` | DAEMON | Disk usage |
| `f` | BAKE_SYNC | same as `docker buildx bake` |
| `history` | NAMESPACE | namespace only |
| `history export` | FILE_BRIDGE | Export build records into Docker Desktop bundle |
| `history import` | LOCAL_ONLY | this reaches into Docker Desktop, which runs on THIS machine and not on the server; run it with plain `docker` here |
| `history inspect` | DAEMON | Inspect a build |
| `history inspect attachment` | DAEMON | Inspect a build record attachment |
| `history logs` | STREAM | Print the logs of a build record |
| `history ls` | DAEMON | List build records |
| `history open` | LOCAL_ONLY | this reaches into Docker Desktop, which runs on THIS machine and not on the server; run it with plain `docker` here |
| `history rm` | DAEMON | Remove build records |
| `history trace` | NOT_YET | trace serves the trace viewer on 127.0.0.1 of whichever machine runs it, so forwarded it binds a port on the server and prints a URL no browser here can open — it wants the tunnel Ulak already builds for Compose, which is not wired to it yet |
| `imagetools` | NAMESPACE | namespace only |
| `imagetools create` | DAEMON | Create a new image based on source images |
| `imagetools inspect` | DAEMON | Show details of an image in the registry |
| `inspect` | DAEMON | Inspect current builder instance |
| `install` | LOCAL_ONLY | install and uninstall rewrite the `docker builder` alias in ~/.docker/config.json on whichever machine runs them, so forwarded they would edit the SERVER's client config and leave this one alone — which is nobody's intent. Buildx 0.33.0 calls both deprecated in any case: type `docker buildx` directly |
| `ls` | DAEMON | List builder instances |
| `policy` | NAMESPACE | namespace only |
| `policy eval` | NOT_YET | `policy eval` takes a local source and reads the `.rego` policy file beside its Dockerfile, and `policy test` takes a local test path — the same footprint `build` already syncs, not yet wired to these two |
| `policy test` | NOT_YET | `policy eval` takes a local source and reads the `.rego` policy file beside its Dockerfile, and `policy test` takes a local test path — the same footprint `build` already syncs, not yet wired to these two |
| `prune` | DAEMON | Remove build cache |
| `rm` | DAEMON | Remove one or more builder instances |
| `stop` | DAEMON | Stop builder instance |
| `uninstall` | LOCAL_ONLY | install and uninstall rewrite the `docker builder` alias in ~/.docker/config.json on whichever machine runs them, so forwarded they would edit the SERVER's client config and leave this one alone — which is nobody's intent. Buildx 0.33.0 calls both deprecated in any case: type `docker buildx` directly |
| `use` | DAEMON | Set the current builder instance |
| `version` | DAEMON | Show buildx version information |

### docker checkpoint — Manage checkpoints

| Command | Route | Notes |
| --- | --- | --- |
| `create` | DAEMON | Create a checkpoint from a running container |
| `list` | DAEMON | same as `docker checkpoint ls` |
| `ls` | DAEMON | List checkpoints for a container |
| `remove` | DAEMON | same as `docker checkpoint rm` |
| `rm` | DAEMON | Remove a checkpoint |

### docker compose — Docker Compose

| Command | Route | Notes |
| --- | --- | --- |
| `alpha` | COMPOSE |  |
| `alpha generate` | COMPOSE | Generate a Compose file from existing containers |
| `alpha publish` | COMPOSE | Publish compose application |
| `alpha viz` | COMPOSE | Generate a graphviz graph from your compose file |
| `attach` | COMPOSE | Attach local standard input, output, and error streams to a service's running container |
| `bridge` | COMPOSE | Convert compose files into another model |
| `bridge convert` | COMPOSE | Convert compose files to Kubernetes manifests |
| `bridge transformations` | COMPOSE | Manage transformation images |
| `bridge transformations create` | COMPOSE | Create a new transformation |
| `bridge transformations list` | COMPOSE | List available transformations |
| `bridge transformations ls` | COMPOSE | List available transformations |
| `build` | COMPOSE | Build or rebuild services |
| `commit` | COMPOSE | Create a new image from a service container's changes |
| `config` | COMPOSE | Parse, resolve and render compose file in canonical format |
| `cp` | COMPOSE | Copy files/folders between a service container and the local filesystem |
| `create` | COMPOSE | Creates containers for a service |
| `down` | COMPOSE | Stop and remove containers, networks |
| `events` | COMPOSE | Receive real time events from containers |
| `exec` | COMPOSE | Execute a command in a running container |
| `export` | COMPOSE | Export a service container's filesystem as a tar archive |
| `images` | COMPOSE | List images used by the created containers |
| `kill` | COMPOSE | Force stop service containers |
| `logs` | COMPOSE | View output from containers |
| `ls` | COMPOSE | List running compose projects |
| `pause` | COMPOSE | Pause services |
| `port` | COMPOSE | Print the public port for a port binding |
| `ps` | COMPOSE | List containers |
| `publish` | COMPOSE | Publish compose application |
| `pull` | COMPOSE | Pull service images |
| `push` | COMPOSE | Push service images |
| `restart` | COMPOSE | Restart service containers |
| `rm` | COMPOSE | Removes stopped service containers |
| `run` | COMPOSE | Run a one-off command on a service |
| `scale` | COMPOSE | Scale services |
| `start` | COMPOSE | Start services |
| `stats` | COMPOSE | Display a live stream of container(s) resource usage statistics |
| `stop` | COMPOSE | Stop services |
| `top` | COMPOSE | Display the running processes |
| `unpause` | COMPOSE | Unpause services |
| `up` | COMPOSE | Create and start containers |
| `version` | COMPOSE | Show the Docker Compose version information |
| `volumes` | COMPOSE | List volumes used by the created containers |
| `wait` | COMPOSE | Block until containers of all (or specified) services stop |
| `watch` | COMPOSE | Watch build context for service and rebuild/refresh containers when files are updated |

### docker config — Manage Swarm configs

| Command | Route | Notes |
| --- | --- | --- |
| `create` | FILE_BRIDGE | Create a config from a file or STDIN |
| `inspect` | DAEMON | Display detailed information on one or more configs |
| `list` | DAEMON | same as `docker config ls` |
| `ls` | DAEMON | List configs |
| `remove` | DAEMON | same as `docker config rm` |
| `rm` | DAEMON | Remove one or more configs |

### docker container — Manage containers

| Command | Route | Notes |
| --- | --- | --- |
| `attach` | STREAM | Attach local standard input, output, and error streams to a running container |
| `commit` | DAEMON | Create a new image from a container's changes |
| `cp` | FILE_BRIDGE | Copy files/folders between a container and the local filesystem |
| `create` | FOOTPRINT_SYNC | Create a new container |
| `diff` | DAEMON | Inspect changes to files or directories on a container's filesystem |
| `exec` | STREAM | Execute a command in a running container |
| `export` | FILE_BRIDGE | Export a container's filesystem as a tar archive |
| `inspect` | DAEMON | Display detailed information on one or more containers |
| `kill` | DAEMON | Kill one or more running containers |
| `list` | DAEMON | same as `docker container ls` |
| `logs` | STREAM | Fetch the logs of a container |
| `ls` | DAEMON | List containers |
| `pause` | DAEMON | Pause all processes within one or more containers |
| `port` | DAEMON | List port mappings or a specific mapping for the container |
| `prune` | DAEMON | Remove all stopped containers |
| `ps` | DAEMON | same as `docker container ls` |
| `remove` | DAEMON | same as `docker container rm` |
| `rename` | DAEMON | Rename a container |
| `restart` | DAEMON | Restart one or more containers |
| `rm` | DAEMON | Remove one or more containers |
| `run` | FOOTPRINT_SYNC | Create and run a new container from an image |
| `start` | STREAM | Start one or more stopped containers |
| `stats` | STREAM | Display a live stream of container(s) resource usage statistics |
| `stop` | DAEMON | Stop one or more running containers |
| `top` | DAEMON | Display the running processes of a container |
| `unpause` | DAEMON | Unpause all processes within one or more containers |
| `update` | DAEMON | Update configuration of one or more containers |
| `wait` | STREAM | Block until one or more containers stop, then print their exit codes |

### docker context — Manage contexts

| Command | Route | Notes |
| --- | --- | --- |
| `create` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `export` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `import` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `inspect` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `list` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `ls` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `remove` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `rm` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `show` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `update` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |
| `use` | LOCAL_ONLY | Docker contexts are this machine's client state; Ulak picks the server from the workspace config instead |

### docker image — Manage images

| Command | Route | Notes |
| --- | --- | --- |
| `build` | BUILD_SYNC | Build an image from a Dockerfile |
| `history` | DAEMON | Show the history of an image |
| `import` | FILE_BRIDGE | Import the contents from a tarball to create a filesystem image |
| `inspect` | DAEMON | Display detailed information on one or more images |
| `list` | DAEMON | same as `docker image ls` |
| `load` | FILE_BRIDGE | Load an image from a tar archive or STDIN |
| `ls` | DAEMON | List images |
| `prune` | DAEMON | Remove unused images |
| `pull` | DAEMON | Download an image from a registry |
| `push` | DAEMON | Upload an image to a registry |
| `remove` | DAEMON | same as `docker image rm` |
| `rm` | DAEMON | Remove one or more images |
| `rmi` | DAEMON | same as `docker image rm` |
| `save` | FILE_BRIDGE | Save one or more images to a tar archive |
| `tag` | DAEMON | Create a tag TARGET_IMAGE that refers to SOURCE_IMAGE |

### docker manifest — Manage Docker image manifests and manifest lists

| Command | Route | Notes |
| --- | --- | --- |
| `annotate` | LOCAL_ONLY | a manifest list is assembled in ~/.docker/manifests on the machine that runs the command and pushed straight to the registry — no daemon takes part, which is why forwarding these looked harmless. Forwarded, the list is built in the SERVER's store, where the `docker manifest push` you type next cannot find it; run them here with plain `docker` |
| `create` | LOCAL_ONLY | a manifest list is assembled in ~/.docker/manifests on the machine that runs the command and pushed straight to the registry — no daemon takes part, which is why forwarding these looked harmless. Forwarded, the list is built in the SERVER's store, where the `docker manifest push` you type next cannot find it; run them here with plain `docker` |
| `inspect` | DAEMON | Display an image manifest, or manifest list |
| `push` | LOCAL_ONLY | a manifest list is assembled in ~/.docker/manifests on the machine that runs the command and pushed straight to the registry — no daemon takes part, which is why forwarding these looked harmless. Forwarded, the list is built in the SERVER's store, where the `docker manifest push` you type next cannot find it; run them here with plain `docker` |
| `rm` | LOCAL_ONLY | a manifest list is assembled in ~/.docker/manifests on the machine that runs the command and pushed straight to the registry — no daemon takes part, which is why forwarding these looked harmless. Forwarded, the list is built in the SERVER's store, where the `docker manifest push` you type next cannot find it; run them here with plain `docker` |

### docker network — Manage networks

| Command | Route | Notes |
| --- | --- | --- |
| `connect` | DAEMON | Connect a container to a network |
| `create` | DAEMON | Create a network |
| `disconnect` | DAEMON | Disconnect a container from a network |
| `inspect` | DAEMON | Display detailed information on one or more networks |
| `list` | DAEMON | same as `docker network ls` |
| `ls` | DAEMON | List networks |
| `prune` | DAEMON | Remove all unused networks |
| `remove` | DAEMON | same as `docker network rm` |
| `rm` | DAEMON | Remove one or more networks |

### docker node — Manage Swarm nodes

| Command | Route | Notes |
| --- | --- | --- |
| `demote` | DAEMON | Demote one or more nodes from manager in the swarm |
| `inspect` | DAEMON | Display detailed information on one or more nodes |
| `list` | DAEMON | same as `docker node ls` |
| `ls` | DAEMON | List nodes in the swarm |
| `promote` | DAEMON | Promote one or more nodes to manager in the swarm |
| `ps` | DAEMON | List tasks running on one or more nodes, defaults to current node |
| `remove` | DAEMON | same as `docker node rm` |
| `rm` | DAEMON | Remove one or more nodes from the swarm |
| `update` | DAEMON | Update a node |

### docker plugin — Manage plugins

| Command | Route | Notes |
| --- | --- | --- |
| `create` | NOT_YET | plugin create reads a local rootfs directory that Ulak does not sync yet — build the plugin on the server, or push it to a registry and `plugin install` from there |
| `disable` | DAEMON | Disable a plugin |
| `enable` | DAEMON | Enable a plugin |
| `inspect` | DAEMON | Display detailed information on one or more plugins |
| `install` | STREAM | Install a plugin |
| `list` | DAEMON | same as `docker plugin ls` |
| `ls` | DAEMON | List plugins |
| `push` | DAEMON | Push a plugin to a registry |
| `remove` | DAEMON | same as `docker plugin rm` |
| `rm` | DAEMON | Remove one or more plugins |
| `set` | DAEMON | Change settings for a plugin |
| `upgrade` | STREAM | Upgrade an existing plugin |

### docker secret — Manage Swarm secrets

| Command | Route | Notes |
| --- | --- | --- |
| `create` | FILE_BRIDGE | Create a secret from a file or STDIN |
| `inspect` | DAEMON | Display detailed information on one or more secrets |
| `list` | DAEMON | same as `docker secret ls` |
| `ls` | DAEMON | List secrets |
| `remove` | DAEMON | same as `docker secret rm` |
| `rm` | DAEMON | Remove one or more secrets |

### docker service — Manage Swarm services

| Command | Route | Notes |
| --- | --- | --- |
| `create` | DAEMON | Create a new service |
| `inspect` | DAEMON | Display detailed information on one or more services |
| `list` | DAEMON | same as `docker service ls` |
| `logs` | STREAM | Fetch the logs of a service or task |
| `ls` | DAEMON | List services |
| `ps` | DAEMON | List the tasks of one or more services |
| `remove` | DAEMON | same as `docker service rm` |
| `rm` | DAEMON | Remove one or more services |
| `rollback` | DAEMON | Revert changes to a service's configuration |
| `scale` | DAEMON | Scale one or multiple replicated services |
| `update` | DAEMON | Update a service |

### docker stack — Manage Swarm stacks

| Command | Route | Notes |
| --- | --- | --- |
| `config` | STACK_SYNC | Outputs the final config file, after doing merges and interpolations |
| `deploy` | STACK_SYNC | Deploy a new stack or update an existing stack |
| `down` | DAEMON | same as `docker stack rm` |
| `list` | DAEMON | same as `docker stack ls` |
| `ls` | DAEMON | List stacks |
| `ps` | DAEMON | List the tasks in the stack |
| `remove` | DAEMON | same as `docker stack rm` |
| `rm` | DAEMON | Remove one or more stacks |
| `services` | DAEMON | List the services in the stack |
| `up` | STACK_SYNC | same as `docker stack deploy` |

### docker swarm — Manage Swarm

| Command | Route | Notes |
| --- | --- | --- |
| `ca` | DAEMON | Display and rotate the root CA |
| `init` | DAEMON | Initialize a swarm |
| `join` | DAEMON | Join a swarm as a node and/or manager |
| `join-token` | DAEMON | Manage join tokens |
| `leave` | DAEMON | Leave the swarm |
| `unlock` | STREAM | Unlock swarm |
| `unlock-key` | DAEMON | Manage the unlock key |
| `update` | DAEMON | Update the swarm |

### docker system — Manage Docker

| Command | Route | Notes |
| --- | --- | --- |
| `df` | DAEMON | Show docker disk usage |
| `dial-stdio` | STREAM | Proxy the daemon socket to stdio |
| `events` | STREAM | Get real time events from the server |
| `info` | DAEMON | Display system-wide information |
| `prune` | DAEMON | Remove unused data |

### docker volume — Manage volumes

| Command | Route | Notes |
| --- | --- | --- |
| `create` | DAEMON | Create a volume |
| `inspect` | DAEMON | Display detailed information on one or more volumes |
| `list` | DAEMON | same as `docker volume ls` |
| `ls` | DAEMON | List volumes |
| `prune` | DAEMON | Remove unused local volumes |
| `remove` | DAEMON | same as `docker volume rm` |
| `rm` | DAEMON | Remove one or more volumes |
| `update` | DAEMON | Update a volume (cluster volumes only) |
