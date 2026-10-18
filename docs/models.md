# Setting up models

Two harnesses, one provider, and a stale catalog in between. Everything here
was measured on 2026-10-18; the numbers are reproducible with the commands
shown.

## The trap: opencode's bundled NVIDIA catalog is stale

opencode ships a hardcoded NVIDIA model list. It does **not** match the live
endpoint. Two consequences:

- Models that exist are **invisible**. `stepfun-ai/step-5-preview` is absent
  from opencode's catalog entirely, so no amount of whitelisting will show it.
- Models listed in the catalog have **disappeared upstream**. The live endpoint
  returns 81 models; of 9 sampled from the old list, 7 answered `410 Gone` or
  `404`. A whitelisted name that matches nothing is silently ignored.

A provider that lists its models explicitly is immune to both. That is why
`nvidia-live` exists.

```
nvidia-live/stepfun-ai/step-5-preview          6.1s   PONG
```

## `nvidia-live` in `opencode.jsonc`

```jsonc
"nvidia-live": {
  "npm": "@ai-sdk/openai-compatible",
  "name": "NVIDIA (live)",
  "options": {
    "baseURL": "https://integrate.api.nvidia.com/v1"
  },
  "models": {
    "stepfun-ai/step-5-preview":          { "name": "Step 5 Preview" },
    "nvidia/nemotron-3-super-120b-a12b":  { "name": "Nemotron 3 Super 120B" },
    "nvidia/nemotron-3-nano-30b-a3b":     { "name": "Nemotron 3 Nano 30B" },
    "z-ai/glm-5.3-flash":                 { "name": "GLM 5.3 Flash" },
    "deepseek-ai/deepseek-v4.1-flash":    { "name": "DeepSeek V4.1 Flash" }
  }
}
```

Three details that each cost a failed request to discover:

**No `apiKey` option.** An explicit `apiKey` wins over the credential store.
`{env:NVIDIA_API_KEY}` is read once at opencode startup, so inside an
already-running opencode it resolves to empty and the request goes out
unauthenticated: `401 Header of type 'authorization' was missing`. With the
option absent, opencode resolves the key from
`~/.local/share/opencode/auth.json` under this provider's id.

**A new `auth.json` entry needs an opencode restart.** The credential store is
cached at startup, unlike the config file, which is re-read. Adding
`nvidia-live` to a running opencode yields 401 until it is restarted.

**The id is `stepfun-ai/...`, not `stepfun/...`.** The short form is an alias
some harnesses accept. It is not what the endpoint returns, so a whitelist
built from aliases matches nothing.

## `experimental.policies` is a deny-by-default allowlist

```jsonc
"policies": [
  { "action": "provider.use", "effect": "deny",  "resource": "*" },
  { "action": "provider.use", "effect": "allow", "resource": "nvidia-live" }
]
```

A new provider is **denied** until it is named in an `allow` entry. The models
list will still show it, which makes this look like an auth problem when it is
not.

## Why `provider` and not `providers`

opencode marks the V1 `provider` key deprecated in favour of V2 `providers`,
but V2 has no whitelist: its `models` map *adds* models rather than
restricting them, so migrating would expose everything. Measured against
openrouter:

| config | total models | curated |
|---|---|---|
| `provider` + `whitelist` | 15 | yes |
| `providers` (V2) alone | 385 | no |
| both | 385 | whitelist silently ignored |

The two keys cannot coexist and the conflict is silent. V1 `provider` is used
throughout, including for `nvidia-live`, where the explicit `models` map is the
point rather than a migration.

## omp model roles

`~/.omp/agent/config.yml` maps four roles onto models verified end to end:

| role | model | measured |
|---|---|---|
| `smol` | `nemotron-3-nano-30b` | 27.6s |
| `default` | `step-5-preview` | 44.0s |
| `slow` | `nemotron-3-super-120b` | 65.7s |
| `plan` | `step-5-preview` | verified |

omp's catalog is bundled and static, so it cannot be widened the way opencode's
can. A role pointing at a model the harness does not know fails at launch, not
at call time.

## Known dead

- `nvidia/llama-3.1-nemotron-ultra-253b-v1` — `404 Function not found for
  account`. Needs enabling at build.nvidia.com; it cannot be fixed from here.
- `z-ai/glm-5.3` — times out at 60s on a bare prompt. `glm-5.3-flash` is fine.
- `deepseek-ai/deepseek-v4.1-flash` — times out at 60s on a bare prompt.
- `stepfun/step-3.7-flash` — `410 Gone`.