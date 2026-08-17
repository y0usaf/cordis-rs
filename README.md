# cordis-rs

A Rust reimplementation of [Cordis](https://github.com/cordiverse/cordis) — the
meta-framework of **spatiotemporal composability**. Plugins are **WASM modules**
run on a `wasmtime` boundary, not Lua.

Inspired by [Cordis](https://github.com/cordiverse/cordis) by Shigma and the
paper [_A Programming Paradigm for Spatiotemporal Composability_](https://github.com/cordiverse/paper).

The paradigm, in one sentence: **every context transformation carries an
inverse the runtime tracks and applies in reverse on unmount (temporal), and a
component declares the context keys it reads so the runtime notifies exactly its
consumers on change (spatial).**

The formal spec is [the paper](https://github.com/cordiverse/paper), parsed here
as [`docs/paper.md`](docs/paper.md).

---

## The two mechanisms

| Paper concept | Plain meaning | Rust home |
|---|---|---|
| **Revertible effect** (§3.1) | every guest write records its inverse (the previous value, or absence); unmount restores them in reverse | `Plugin::inverses`, `Context::rollback` |
| **Reactive coeffect** (§3.2) | a plugin declares the keys it reads (`ctx_read`); a committed `set` notifies exactly those plugins | `State::readers`, `Context::set`/`notify` |
| **Single context type** (§3.3) | effects mutate it, coeffects name its keys — one host-owned state | `State::values` |

## The kernel

`src/lib.rs` — one host-owned context, WASM plugins, single-threaded:

- **`Context`** — `new`, `mount(&[u8])`, `unmount(id)`, `set(key, val)`,
  `get(key)`, `has(key)`. Values are strings. `mount`/`unmount`/`set` return
  `Result`.
- **Temporal** — the kernel records an inverse for every write a plugin makes
  (`ctx_set`/`ctx_remove` store the previous value, or `None`). `unmount`
  replays them in reverse, so the context returns to its pre-mount state.
  Revertibility is kernel-owned, never trusted to the guest.
- **Spatial** — a plugin declares the keys it reads via the `ctx_read` host
  function. A committed [`Context::set`] notifies exactly the plugins that
  declared that key, by writing the key into their scratch buffer and calling
  their `on_change` export.
- **Two-tier writes** — guest `ctx_set`/`ctx_remove` are *effects* (no
  notification); host [`Context::set`] is the *committed* write that notifies
  readers. The context is host-owned; plugins touch it only through
  access-only host functions.

### The WASM boundary

A plugin is a WASM module that imports host functions and exports a lifecycle.

**Host functions** (imported by the guest under the `"host"` module):

| Function | Signature | Meaning |
|---|---|---|
| `ctx_set` | `(kp, kl, vp, vl)` | write `key=value` as an effect; records the inverse |
| `ctx_remove` | `(kp, kl)` | remove `key` as an effect; records the inverse |
| `ctx_read` | `(kp, kl)` | declare `key` as a read (coeffect) |

**Guest exports** (required):

| Export | Signature | Meaning |
|---|---|---|
| `mount` | `() -> ()` | run effects and declare reads at mount |
| `on_change` | `(ptr, len) -> ()` | invoked when a declared key changes |
| `scratch` | `() -> (ptr, cap)` | reserve a buffer the host writes the changed key into |
| `memory` | — | exported linear memory for the scratch buffer |

All guest pointers and lengths are validated (`read_str`): no negative
ptr/len, no out-of-bounds read, no non-UTF-8. Bad input traps, never panics.
A `mount` that traps rolls back every write the plugin made, leaving the
context unchanged.

## Run

```
cargo test    # wasm_compose.rs: mount reverts, coeffect notifies, failed mount rolls back
nix build     # Nix verification
```

## Layout

```
src/lib.rs             kernel: Context, Plugin, inverse tracking, coeffect graph
tests/wasm_compose.rs  end-to-end WASM proof (temporal + spatial + rollback)
docs/paper.md          the paper, parsed to Markdown
ref/                   upstream reference (cordis + paper), not tracked
```

## Design notes

- **`wasmtime` only.** The one runtime dependency is `wasmtime` (Cranelift).
  `wat` is a dev-dependency for authoring test modules.
- **Strings, not `Any`.** Values are `String`; the WASM boundary is
  string-keyed and string-valued by design.
- **Kernel-owned reversibility.** Inverses are tracked host-side, so a
  misbehaving or trapping guest cannot leak state — the host restores it.
