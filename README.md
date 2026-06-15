# Rust Toy OpenAI Server on Baseten

A minimal experiment: a dependency-free Rust binary that answers the standard
OpenAI endpoints with **pre-canned responses**, packaged into a Baseten image via
Truss `build_commands`, on the smallest base image Truss will accept.

It mirrors the SGLang Truss in `../00_first_model` but swaps the GPU model server
for a tiny CPU binary — useful for testing Baseten wiring, autoscaling, routing,
and client code without paying for a GPU or loading real weights.

## Layout

```
rust_experiment/
├── server/                 # Rust source (std-only, no crates)
│   ├── Cargo.toml
│   └── src/main.rs
├── data/                   # Truss bundles this into the image at /app/data
│   └── toy-openai-server   # prebuilt static musl binary (built by build.sh)
├── build.sh                # compile -> stage binary in data/
├── config.yaml             # Truss config: docker_server + build_commands (nginx/supervisord proxy)
├── config.nobuild.yaml     # Truss config: no_build variant (no proxy, ~45% faster cold start)
└── README.md
```

## Build & deploy

```bash
./build.sh                  # compiles a static musl binary into data/

cd /path/to/00_first_model && source venv/bin/activate   # truss CLI lives here
cd -                                                      # back to rust_experiment
truss push . --publish
```

## Endpoints (all canned)

| Method | Path                      | Notes                                  |
|--------|---------------------------|----------------------------------------|
| GET    | `/health`                 | readiness + liveness probe             |
| GET    | `/v1/models`              | model list                             |
| POST   | `/v1/chat/completions`    | supports `"stream": true` (SSE)        |
| POST   | `/v1/completions`         | legacy text completion                 |
| POST   | `/v1/embeddings`          | fixed 8-dim embedding                  |

Anything else returns a JSON 404.

## How the binary gets into the image (`build_commands`)

Truss renders a Dockerfile (`truss image build-context <dst> .` to inspect it).
The key ordering, verified from the generated Dockerfile:

```dockerfile
COPY ./data /app/data            # <-- data/ is copied in BEFORE build_commands
...
RUN mkdir -p /app/bin            # <-- our build_commands
RUN cp /app/data/toy-openai-server /app/bin/toy-openai-server
RUN chmod +x /app/bin/toy-openai-server
...
RUN apt-get install nginx        # truss injects its proxy layer afterwards
ENTRYPOINT supervisord ...
```

So the recipe is: stage the prebuilt binary in `data/`, then `mkdir`/`cp`/`chmod`
it into place with `build_commands`. (`build_commands` are plain `RUN` lines — they
cannot `COPY` from your laptop, but they *can* `cp` from `/app/data`, which Truss
has already populated.)

The binary is **static musl** on purpose: it depends on no libc, so it runs on any
Debian base regardless of glibc version, and stays ~450 KB.

## Endpoint reachability on Baseten — nothing special required

`docker_server.predict_endpoint` is required and maps Baseten's `…/predict` route,
but you are **not** limited to a single endpoint. Baseten forwards **every** route
to your server via the `sync` path:

| Baseten URL                                             | Hits your server at        |
|---------------------------------------------------------|----------------------------|
| `…/sync/v1/chat/completions`                            | `/v1/chat/completions`     |
| `…/sync/v1/models`, `…/sync/v1/embeddings`, etc.        | same path, unchanged       |
| `…/predict`                                             | your `predict_endpoint`    |

So OpenAI clients just point `base_url` at `…/environments/production/sync/v1`
(exactly like `../00_first_model/call_works.py`) and every endpoint works. The only
Baseten-side requirement is that `readiness_endpoint` / `liveness_endpoint` return
200 — Truss' nginx rewrites `GET /` → liveness and `GET /v1/models/model` →
readiness, both pointed at our `/health`.

Source for the routing behavior:
https://docs.baseten.co/truss/guides/custom-server ("you can access any route
exposed by your server using the sync endpoint … All other paths reach your server
unchanged").

## Smallest base image: `python:3.13-slim-bookworm`

This is the floor for a Truss custom server, and the constraint comes from **Truss,
not from our binary**. Truss renders a fail-fast check into every image
(`base.Dockerfile.jinja`, `{% block fail_fast %}` in
https://github.com/basetenlabs/truss):

```dockerfile
RUN grep -w 'ID=debian\|ID_LIKE=debian' /etc/os-release \
  || { echo "ERROR: Supplied base image is not a debian image"; exit 1; }
RUN $(which python3) -c "...3.9 <= python <= 3.14..." \
  || { echo "ERROR: ... does not have 3.9 <= python <= 3.14"; exit 1; }
```

Consequences:

- **Must be Debian/Debian-like** → `alpine`, `scratch`, `distroless`, Wolfi are
  rejected at build time.
- **Python (3.9–3.14) must already be present**, and this check runs *before*
  `build_commands`, so you cannot `apt install python3` to satisfy it. That rules
  out bare `debian:*-slim` (no Python).
- On top of the base, Truss installs `curl` + `nginx` and a uv-managed Python venv
  to run its supervisord/nginx proxy — so a static binary alone can't shrink it.

Note: the Debian/Python requirement is **not in the Baseten docs** — it's only
enforced in the open-source Truss image builder (linked above).

### Measured sizes (local `docker build`)

| Image                         | Size   |
|-------------------------------|--------|
| `python:3.13-slim-bookworm`   | 121 MB |
| **final `toy-openai` image**  | 316 MB |

The ~195 MB delta is Truss' own proxy layer (nginx + a second uv-installed Python +
supervisor), not our 0.45 MB binary. This overhead is fixed *in the default
docker_server mode* — but `no_build` removes it entirely (see below).

## `no_build`: drop the proxy layer (faster cold starts)

`docker_server.no_build: true` makes Truss skip its whole in-container proxy stack.
It renders a completely different, ~4-line Dockerfile (no nginx, no supervisord, no
extra Python, no Debian/Python `fail_fast` check, no `build_commands`):

```dockerfile
FROM <base>
COPY ./data /app/data
ENTRYPOINT ["sh","-c"]
CMD ["<start_command>"]
```

Your binary is the **only process** and the entrypoint. The request routing nginx
did in-container moves *out* to Baseten's infrastructure: path routing is **1:1**
(no `predict_endpoint` remap), and readiness/liveness probes hit your endpoints
directly on `server_port`. Deploy it with:

```bash
cp config.nobuild.yaml config.yaml && truss push .   # then restore config.yaml
```

### Cold-start comparison (measured on Baseten, CPU 1x2, scale-to-zero)

| | Warm node (image cached) | Cold node (uncached pull) | Image |
|---|---|---|---|
| `config.yaml` (proxy)            | **5.306 s** | ~20 s  | 188 MB / 15.6 s pull |
| `no_build` on `python:3.13-slim` | **2.914 s** | ~3.5 s | 51 MB / 0.59 s pull  |
| `config.nobuild.yaml` (Alpine)   | **2.567 s** | ~3 s   | 22 MB / 0.35–6 s pull |

Where the ~2.4 s warm-node saving comes from — the `scaling up → server listening`
span in the replica logs:
- proxy mode: **~2.2 s** (boot supervisord → its Python 3.14 → nginx → spawn procs)
- `no_build`: **44 ms** (kernel execs the binary, it binds the socket)

The irreducible floor (identical in both modes) is Baseten pod scheduling
(~1.1 s) + the binary's own boot (~40–130 ms). Everything `no_build` removes is
the Truss proxy stacked on top of that floor.

### Instance type barely matters for *this* workload (CPU vs L4 GPU)

`no_build` on `python:3.13-alpine` (22 MB), verified against replica logs:

| | CPU (1x2) | L4:1 GPU |
|---|---|---|
| Cold start (scale-from-zero, first call) | **2.567 s** | **2.889 s** |
| └ compute acquired | +1.214 s | +1.387 s |
| └ binary `listening` | +2.053 s | +2.607 s |
| Fresh deploy (server-side, build→success) | ~18 s | **22.4 s** |
| └ of which GPU/CPU node scheduling | ~1.8 s | ~3.9 s |

The GPU instance adds only ~0.3 s to the cold start **because our binary never
initializes the GPU** — no CUDA context, no weights to load into VRAM. The cold
start is the same pod-schedule → image-cache → exec sequence regardless of
instance type. (`B200` was not available on the test org — *"contact support to
request this instance type"* — so L4 stands in for the GPU case.)

> Caveat for real models: a real GPU server's cold start is dominated by CUDA init
> + loading weights into VRAM, which is the **model**, not the infra. This toy
> binary isolates the infra component, and that part is instance-agnostic (~2.6–2.9 s).
>
> Image-pull time is noisy (registry/node variance): the same 22 MB image pulled in
> 0.35 s, 0.59 s, and 6.2 s across runs — not size-proportional.

### `no_build` tradeoffs

- **`build_commands` are ignored** → stage the binary in `data/` and `chmod +x` it in
  `start_command` (`chmod +x … && exec …`).
- **1:1 routing** → `predict_endpoint` has no effect; the server must expose the
  OpenAI paths and own `/health` directly (ours does).
- **`server_port` must not be 8080.**
- **Baseten's push-time validation requires the base image to contain Python.**
  This is a *platform* check (separate from Truss' `fail_fast`, which `no_build`
  skips). We mapped it with validation-only pushes:

  | Base image | Debian? | Python? | Result |
  |---|---|---|---|
  | `python:3.13-slim-bookworm` | yes | yes | ✅ accepted (51 MB pulled) |
  | `python:3.13-alpine`        | no  | yes | ✅ accepted (**22 MB** pulled) |
  | `debian:bookworm-slim`      | yes | no  | ❌ rejected |
  | `busybox:musl`              | no  | no  | ❌ rejected |

  The gate keys on **Python, not Debian** — Alpine is fine if it carries Python.
  So the smallest accepted base is **`python:3.13-alpine`** (our static musl binary
  runs natively on it). `busybox`/`scratch`/`debian-slim` are rejected with
  *"Custom base images not supported for your organization"* (a misleading message —
  it's really "this base has no Python"). The 2 MB busybox image builds and runs
  locally but can't deploy without that entitlement.

## Local verification (done)

- `cargo build --release --target x86_64-unknown-linux-musl` → 453 KB static binary
  (`ldd` → "statically linked").
- Ran the binary directly and curled every endpoint (incl. SSE stream + 404).
- `docker build` of the Truss-generated context succeeded (fail-fast checks passed,
  `build_commands` ran).
- Ran the final container and curled through the nginx proxy on `:8080`: liveness
  (`/`) and readiness (`/v1/models/model`) both rewrite to `/health` → 200, and all
  `/v1/...` routes pass through correctly.
- Built the `no_build` image locally on `busybox:musl` → **2 MB**, ran it, all
  endpoints served directly on `:8000` (no proxy).

## Baseten verification (done, then torn down)

- Deployed both variants to Baseten; all OpenAI endpoints worked live via the
  `…/sync/v1` path, including SSE streaming.
- Measured the cold starts in the table above from client timing + replica logs.
- Hit two real gotchas, both Baseten-specific (not in the binary):
  1. **`PORT=8080` collision** — Baseten injects `PORT=8080` (the proxy's port). The
     server must bind `server_port` (8000), never the generic `PORT`. `main.rs` reads
     `TOY_SERVER_PORT` instead. (Passed locally only because no `PORT` was set there.)
  2. **No auto-promotion** — re-`truss push` after a failed deploy creates a new
     deployment but doesn't promote it over the failed production slot. Promote via
     `POST …/v1/models/{id}/deployments/{dep}/promote`.
- All experiment models were deleted afterward.

- `cargo build --release --target x86_64-unknown-linux-musl` → 453 KB static binary
  (`ldd` → "statically linked").
- Ran the binary directly and curled every endpoint (incl. SSE stream + 404).
- `docker build` of the Truss-generated context succeeded (fail-fast checks passed,
  `build_commands` ran).
- Ran the final container and curled through the nginx proxy on `:8080`: liveness
  (`/`) and readiness (`/v1/models/model`) both rewrite to `/health` → 200, and all
  `/v1/...` routes pass through correctly.
