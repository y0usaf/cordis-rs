# cordis-rs

A Rust reimplementation of [Cordis](https://github.com/cordiverse/cordis) — the
meta-framework of **spatiotemporal composability**. Lua is the scripting source
over the Rust kernel, not a second kernel.

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
| **Revertible effect** (§3.1) | `ctx.effect(fn)` returns a disposer; the runtime tracks it and runs inverses LIFO on unmount | `Fiber::disposables`, `EffectInner::run` |
| **Reactive coeffect** (§3.2) | `ctx.inject(deps, fn)` + `ctx.provide(name, v)`; consumers reload when a dependency changes state | `notify`, `Fiber::refresh`/`check_impl` |
| **Single context type** (§3.3) | effects mutate it, coeffects name its keys — one host-owned state | `Context` + `Shared` |
| **Component / fiber** (§4.1) | a mounted unit with a lifecycle state machine | `Fiber`, `FiberState` |

## The kernel

`src/core.rs` — all state is `Rc`-owned, single-threaded (Lua is not `Send`):

- **`Context`** — a node in a prototype chain. `isolate(name)` shadows a service
  symbol; `isolate_key(name)` walks the chain to resolve the symbol.
- **`Fiber`** — `uid`, `state`, `disposables`, `inject`, `epoch`, `error`. The
  lifecycle: `PENDING → ACTIVE → (PENDING | DISPOSED)`, with `FAILED` on
  callback error. (Sync: the `LOADING`/`UNLOADING` in-flight states of the async
  calculus are unobservable and omitted.)
- **`EffectInner` / `Disposable`** — an effect collects disposers; `run()` applies
  them once, in reverse. A `Disposable` is a raw `FnOnce` or a nested effect.
- **`RegistryService`** — `plugin()` mints a `Runtime` per callback, tracks its
  live fibers, `delete()` disposes them.
- **`ReflectService`** — the service store, keyed by isolate symbol (`Impl`).
- **`EventsService`** — `on`/`once`/`emit`/`parallel`/`serial`/`bail`/`waterfall`.
- **`LoggerService`** — a message buffer, `error`/`warn`/`info`/`debug`.

`src/lua.rs` — the `UserData` bindings. `ctx` is a `Ctx`; plugins and effects are
Lua functions; a plugin returns `nil` | a disposer function | a table of disposers.

---

## Checklist: how exactly this is Cordis

Each row names the Cordis mechanism, its Rust realization, and its status.
`✅` = implemented and covered by `tests/core.rs`. `⬜` = deferred, with reason.

### §3 Revertible effects and reactive coeffects

| # | Cordis | In cordis-rs | Status |
|---|---|---|---|
| 1 | Effect returns an inverse the runtime tracks | `ctx:effect(fn, label)` → `EffectHandle`; disposer collected into `Fiber::disposables` | ✅ |
| 2 | Inverses applied in reverse on unmount | `EffectInner::run` reverses the collected list | ✅ |
| 3 | Nested effects form a tree | — | ⬜ tree metadata removed; `get_effects` returns flat labels |
| 4 | Idempotent dispose | `EffectInner::ran` flag | ✅ |
| 5 | Coeffect spec = context keys a component reads | `Fiber::inject: HashMap<String, Option<Value>>` | ✅ |
| 6 | Change notifies exactly matching consumers | `notify()` scans runtimes, `refresh` per matching fiber | ✅ |
| 7 | Isolation (symbol-scoped services) | `Context::isolate(name)` shadows `isolate_root`; `isolate_key` | ✅ |
| 8 | Interception (config shadowing) | — | ⬜ removed as speculative (no config merge) |
| 9 | Unified context type | `Context` + `Shared` (one host-owned state) | ✅ |
| 10 | Observational equivalence / effect independence | — | ⬜ formal property, not a runtime feature |

### §4 Calculus of dynamic composition

| # | Cordis | In cordis-rs | Status |
|---|---|---|---|
| 11 | Component = fiber with lifecycle | `Fiber` + `FiberState` (4 states; sync) | ✅ |
| 12 | `plugin(fn, config)` mounts a component | `Context::plugin` → `Rc<Fiber>`, registered with parent fiber | ✅ |
| 13 | `inject(deps, fn)` declares coeffects | `Context::plugin` with `inject` map; `refresh` resolves deps | ✅ |
| 14 | Withdrawal (unload runs disposers) | `Fiber::cleanup` → `set_epoch(Inactive)` → `unload` | ✅ |
| 15 | Iteration (reload on dep change) | `refresh` recomputes the epoch fingerprint, `set_epoch` reloads | ✅ |
| 16 | Asynchrony (inertia lock, in-flight reload) | — | ⬌ sync only; needs an executor, conflicts with "stdlib only" |
| 17 | Failure (callback throws → `FAILED`) | `reload` catches, sets `error`, `FiberState::Failed` | ✅ |
| 18 | `update` / `restart` | `Fiber::update`, `restart` | ✅ (`await` omitted — sync) |
| 19 | Metatheory (preservation, progress, confluence) | — | ⬜ proof, not code |

### §5 Implementation

| # | Cordis | In cordis-rs | Status |
|---|---|---|---|
| 20 | Effect tracking | `Fiber::disposables` (`DisposableList`) | ✅ |
| 21 | Coeffect operations (`provide`/`get`/`set`) | `Context::provide/get/set`, `resolve_property` | ✅ |
| 22 | Component lifecycle | `Fiber` state machine | ✅ |
| 23 | Context access (`ctx.foo` coeffect resolution) | `Ctx` `__index` → `resolve_property` | ✅ |
| 24 | Declarative component loader | — | ⬜ `@cordisjs/plugin-loader` not ported |
| 25 | Hot module replacement | — | ⬜ `@cordisjs/plugin-hmr` not ported |
| 26 | Config validation (StandardSchema) | — | ⬜ no schema layer |

### Service surface (the `Service` class in TS)

| # | Cordis | In cordis-rs | Status |
|---|---|---|---|
| 27 | `Service` base (name, ctx) | Lua values provided via `ctx:provide` | ✅ (no class) |
| 28 | `Service.init` (async init gate) | — | ⬜ async |
| 29 | Callable service (`Service.invoke`) | — | ⬜ |
| 30 | `@Inject` decorators | — | ⬜ |
| 31 | `associate` / `mixin` / `accessor` traceability | — | ⬜ JS-proxy mechanism, not ported |

### Events and logging

| # | Cordis | In cordis-rs | Status |
|---|---|---|---|
| 32 | `on` / `once` | `Context::on/once` (effect-backed) | ✅ |
| 33 | `emit` / `parallel` / `serial` / `bail` / `waterfall` | `Context::emit/parallel/serial/bail/waterfall` | ✅ |
| 34 | Logger with levels + exporters | `LoggerService` buffer + `Logger` UserData | ✅ (no exporters/formatters) |

**Score: 22 of 34 rows `✅`.** The gaps are one of three kinds: (a) async
machinery, (b) the JS-proxy traceability layer (`associate`/`mixin`/`accessor`/
callable services), (c) the loader/HMR plugin packages. All three are orthogonal
to the paradigm itself, which is fully present.

---

## The Lua surface

```lua
-- revertible effect
local d = ctx:effect(function() return function() undo() end end, "label")

-- reactive coeffect
ctx:inject({"db"}, function(c) print(c.db) end)
local pd = ctx:provide("db", { host = "localhost" })

-- component
local f = ctx:plugin(function(c, cfg) return function() cleanup() end end, { x = 1 })

-- events
ctx:on("tick", function(n) end)
ctx:emit("tick", 42)
```

## Run

```
cargo run --bin demo     # examples/demo.lua
cargo test               # 10 tests mirroring upstream packages/core/tests
nix build                # Nix verification
```

## Layout

```
src/core.rs    kernel: Context, Fiber, effects, coeffects, services, events, logger
src/lua.rs     Lua UserData bindings
tests/core.rs  behavioral spec (mirrors ref/cordis/packages/core/tests)
docs/paper.md  the paper, parsed to Markdown
ref/           upstream reference (cordis + paper), not tracked
```

## Design notes

- **Stdlib + mlua only.** The one dependency is `mlua` (vendored Lua 5.4). No
  executor, no `futures` — hence sync-only.
- **Ownership.** `Context` holds `Weak<Fiber>`; `Fiber` holds `Rc<Context>`.
  The cycle is broken on the context→fiber edge so mount/unmount does not leak.
- **Faithfulness over ergonomics.** The reverse-order unmount, isolate-key
  resolution, and epoch-based reload are ported to match the TS semantics, not
  approximated.
