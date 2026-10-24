# Setting up models

Two harnesses, one provider, and three different model lists in between. Every
row below was measured, and each says *where* it was measured, because the
answer differs by harness.

`tools/setup-opencode-models.ps1` re-checks the opencode side on demand.

## What actually works

Measured 2026-10-21.

| model | opencode (`nvidia/…`) | omp | notes |
|---|---|---|---|
| `stepfun-ai/step-5-preview` | PONG 11s, 5.6s | PONG | the `default` and `plan` role |
| `z-ai/glm-5.3-flash` | PONG 845s | PONG 2.5s | the `slow` role |
| `deepseek-ai/deepseek-v4.1-flash` | PONG 853s | PONG 46.9s | |
| `nemotron-3-nano-30b-a3b` | not in catalog | PONG 5.9s | the `smol` role |
| `nemotron-3-nano-omni-30b-a3b-reasoning` | not in catalog | PONG 4.5s | |

The two ~850s figures are opencode-specific and were measured on a cold
`--standalone` run; a direct call to the same model answers in ~1s, so the cost
is harness startup, not the model.

## The direct endpoint is not the source of truth

`https://integrate.api.nvidia.com/v1` disagrees with both harnesses:

```
stepfun-ai/step-5-preview              direct: 404    opencode: PONG   omp: PONG
nemotron-3-nano-30b-a3b                direct: 410    omp: PONG
nemotron-3-super-120b-a12b             direct: 404    omp: falls through to openrouter
```

So neither `GET /v1/models` nor a direct `POST /v1/chat/completions` can decide
whether a model is usable. Only the harness can. Do not "fix" a model that
answers in the harness because the raw endpoint rejects it.

## `nemotron-3-super-120b-a12b` is a trap

It was the `slow` role until 2026-10-21. In omp it does not fail loudly: the
name does not resolve in omp's bundled catalog, so fuzzy matching falls through
to the same id on **openrouter**, which answers

```
402 This request requires more credits, or fewer max_tokens.
    You requested up to 131072 tokens, but can only afford 7269.
```

That is a paid wall, so the role was silently unusable while looking configured.
It now points at `z-ai/glm-5.3-flash`. A check in
`tools/setup-opencode-models.ps1` fails if a dead id comes back into any role.

The `openrouter/…:free` variant does not rescue it either — it returns
`Error: Provider returned error`.

## opencode's bundled catalog is stale, and cannot be widened

opencode ships a hardcoded NVIDIA list. The live endpoint returns 83 models, the
bundled list exposes 58, and the difference is not a subset anyone can predict.
Models present in the catalog have also disappeared upstream.

A hand-maintained whitelist disagrees with the catalog silently, so on 2026-10-19
every whitelist was removed and each provider's full catalog is exposed. To hide
a model, add `"disabled": true` under that provider entry.

The alternative — a custom provider id such as `nvidia-live` — was tried and
abandoned, for the credential reason below.

## Credentials key on integration names

opencode's auth system keys on integration *names* (`nvidia`, `mistral`,
`openrouter`, `atria`), not on provider ids. A custom provider id has no matching
integration and therefore gets no credential, which is the whole reason
`nvidia-live` failed with a 401. The standard `nvidia` provider serves every
model that is in its catalog.

`opencode auth export` is the source of truth. `~/.local/share/opencode/auth.json`
is a cache and has been observed stale — holding a different key than the export.

## omp roles

`~/.omp/agent/config.yml`:

| role | model | measured |
|---|---|---|
| `default` | `nvidia/stepfun-ai/step-5-preview:high` | PONG |
| `smol` | `nvidia/nemotron-3-nano-30b-a3b` | 5.9s |
| `slow` | `nvidia/z-ai/glm-5.3-flash` | 2.5s |
| `plan` | `nvidia/stepfun-ai/step-5-preview` | PONG |

omp's catalog is bundled and static, so it cannot be widened the way opencode's
can. Roles are validated against the catalog at call time, and a miss can fall
through to another provider rather than erroring.

Verify a role without touching a running session:

```powershell
omp -p --no-session --cwd "$env:TEMP\omp-probe" --model <id> "Reply with exactly one word: PONG"
```

## Known dead

- `nvidia/llama-3.1-nemotron-ultra-253b-v1` — `404 Function not found for
  account`. Needs enabling at build.nvidia.com; not fixable locally.
- `nvidia/nemotron-3-super-120b-a12b` — 404 direct, openrouter fallback is paid.
- `nvidia/nemotron-3-nano-30b-a3b` — 410 direct, but *works* in omp. Kept for
  `smol`. This is the exception that proves the section above.
- `stepfun/step-3.7-flash` — `410 Gone`.

## Incident: the config is not safe from truncation

On 2026-10-21 at 15:47 `~/.config/opencode/opencode.jsonc` was reduced from
5022 bytes to a 50-byte schema stub — no providers, no policies, no MCP servers,
and no backup alongside it. It was recovered from the recycle bin. An opencode
instance started at 15:47:23 therefore booted with no configuration at all.

`tools/setup-opencode-models.ps1` now fails check 1 if the file is under 1 KB or
has no `provider` block, so this is caught rather than discovered by a missing
model later.
