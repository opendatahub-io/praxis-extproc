# BBR on extra JSON OpenAI paths (Kind e2e + MaaS spike)

Body-based routing uses the same Praxis **pre-chain** as chat: JSON `"model"` → `X-Gateway-Model-Name` → HTTPRoute header match.

## Kind e2e (this repository)

After `make e2e-setup`, the gateway URL is printed by tests (`gateway_url()` → MetalLB on `e2e-gateway`).

### Backend A — `granite-8b` (llm-d-inference-sim)

Simulator HTTP surface ([upstream docs](https://github.com/llm-d/llm-d-inference-sim/blob/main/docs/api.md)):

| Path | Example body |
|------|----------------|
| `POST /v1/chat/completions` | `{"model":"granite-8b","messages":[{"role":"user","content":"hi"}]}` |
| `POST /v1/completions` | `{"model":"granite-8b","prompt":"hi","max_tokens":8}` |
| `POST /v1/embeddings` | `{"model":"granite-8b","input":"hello"}` |
| `POST /v1/responses` | `{"model":"granite-8b","input":"hello"}` |

Expect **HTTP 200** and Praxis headers (`X-Praxis-Version`, etc.).

### Backend B — `bbr-path-stub` (WireMock)

Paths the sim **does not** implement but RHAIRFE lists for JSON BBR qualification:

| Path | Example body |
|------|----------------|
| `POST /v1/moderations` | `{"model":"bbr-path-stub","input":"test"}` |
| `POST /v1/rerank` | `{"model":"bbr-path-stub","query":"q","documents":["a","b"]}` |
| `POST /v1/images/generations` | `{"model":"bbr-path-stub","prompt":"circle","n":1}` |
| `POST /v1/search/query` | `{"model":"bbr-path-stub","query":"hello"}` |

Stub returns **HTTP 200** JSON `{"object":"bbr-path-stub","ok":true}` — proves **routing**, not production model quality.

Automated coverage: `tests/k8s_e2e/json_paths.rs`.

## MaaS + Praxis cluster

Use canonical model ids from `GET /maas-api/v1/models` (not Kind model names). For embeddings on Prow fixtures, add `e2e-embedding-simulated` to `simulator-subscription` + `simulator-access` (see internal runbook).

Paths without a serving stack still return **404** until a real model or stub backend is deployed — that is a **serving** gap, not evidence that BBR failed.

## What this does not prove

- Multipart paths (`/v1/audio/transcriptions`, `/v1/images/edits`) — need form-field `model` (separate track).
- Product guardrails on every suffix — e2e scopes `ai_guardrails` to `/v1/chat/completions` only.
