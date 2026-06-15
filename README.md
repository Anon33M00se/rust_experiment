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
├── config.yaml             # Truss config (docker_server + build_commands)
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
supervisor), not our 0.45 MB binary. That overhead is fixed by Truss' custom-server
design and isn't reducible from config.

## Local verification (done)

- `cargo build --release --target x86_64-unknown-linux-musl` → 453 KB static binary
  (`ldd` → "statically linked").
- Ran the binary directly and curled every endpoint (incl. SSE stream + 404).
- `docker build` of the Truss-generated context succeeded (fail-fast checks passed,
  `build_commands` ran).
- Ran the final container and curled through the nginx proxy on `:8080`: liveness
  (`/`) and readiness (`/v1/models/model`) both rewrite to `/health` → 200, and all
  `/v1/...` routes pass through correctly.
