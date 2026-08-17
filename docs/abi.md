# cordis-rs WASM ABI

One WASM boundary. Two function sets on one linker. All host functions live in cordis-rs — grow once, not per-repo.

## Value model (locked)

All values are `String`. Typed data serializes as JSON. Guest pointers/lengths validated; bad input traps, never panics.

## Guest exports (required, locked)

| Export | Signature |
|---|---|
| `mount` | `() -> ()` |
| `on_change` | `(ptr, len) -> ()` |
| `scratch` | `() -> (ptr, cap)` |
| `memory` | linear memory |

## Function set 1 — core kernel (shared by all)

| Function | Signature | Meaning |
|---|---|---|
| `ctx_set` | `(kp,kl,vp,vl)` | effect write; records inverse |
| `ctx_remove` | `(kp,kl)` | effect remove; records inverse |
| `ctx_read` | `(kp,kl)` | declare read (coeffect) |

Config WASM uses only these three: `mount` calls `ctx_set` to populate config keys. Config *is* an extension that only writes.

## Function set 2 — extension registration (ekko)

| Function | Signature |
|---|---|
| `register_command` | `(name_ptr,name_len,desc_ptr,desc_len)` |
| `register_surface` | `(name_ptr,name_len,dock,priority,size)` |
| `register_keybinding` | `(chord,mode,desc,handler)` |
| `register_mode` | `(name,on_key,init,render)` |
| `register_overlay` | `(name,desc,render,handle_key,init)` |
| `register_theme` | `(name,desc,handler)` |
| `register_spinner` | `(name,handler)` |
| `register_session_grouper` | `(name,handler)` |
| `register_session_namer` | `(name,handler)` |
| `subscribe` | `(event,handler)` |
| `register_action_interpreter` | `(name,desc,handler)` |

## Function set 3 — draw ops (ekko)

`size()->(w,h)`, `fill_rect`, `set_cell`, `put_text`, `put_text_bold`, `put_text_styled`, `draw_box`, `render_scrollbar`. Data-only ops buffered, replayed after clean guest return. Fuel-metered (draw budget 200k, call budget 2M instructions).

## Function set 4 — pi-harness scope (resolved: OUT of cordis-rs)

pi-harness's `pi-extension/` is a JavaScript Pi extension that pi-harness does NOT
execute: it is packaged (`share/pi-harness/pi-extension/index.js`) and passed to the
*external* Pi binary via `-e`, running inside the Pi PTY's Node runtime. The `pi.*`
API surface it uses belongs to that external Pi runtime, not to cordis-rs. It is
therefore OUT of scope for the cordis-rs WASM boundary. pi-harness itself (the Rust
TUI harness) gets a cordis-rs kernel using only core set 1. No `pi.*` host function
set is exposed by cordis-rs.

## Function set 5 — tomoe compositor

Top-level `tomoe.*`: `settings(table)`, `bind(combo,action,desc?)`, `spawn(cmd)`, `clear_focus()`, `quit()`, `windows()`, `window(id)`, `rule(spec)`, `rules_for(win)`, `focused_window()`, `usable_area(idx?)`, `outputs()`, `view()`, `set_view(table)`, `pointer()`, `grab_pointer(motion,release?)`, `ungrab_pointer()`, `on_reload(name,save,restore)`.

`tomoe.process`: `once(id,opts?)`, `service(id,opts?)`, `spawn(opts)`.
`tomoe.ipc`: `serve(method,handler)`, `broadcast(event,payload?)`.
`tomoe.ui`: `confirm(spec)`, `menu(spec)`, `toast(spec)`, `sheet(spec)`.

Event hooks: `on_window_open(fn)`, `on_window_close(fn)`, `on_focus_change(fn)`, `on_outputs_changed(fn)`, `on_pointer_button(fn)`, `on_pointer_axis(fn)`, `on_pointer_enter(fn)`, `on_pointer_leave(fn)`, `on_window_request(fn)`, `on_screencast_request(fn)`.

Window methods: `id`, `app_id`, `title`, `is_mapped`, `is_focused`, `is_fullscreen`, `is_maximized`, `geometry`, `set_properties`, `show`, `hide`, `focus`, `raise`, `set_fullscreen`, `set_maximized`, `close`.

## Function set 6 — host->guest dispatch (dynamic callbacks) — IMPLEMENTED + TESTED

Restores the live-callback surface that Lua used to own (which-key's key hook,
modes, status bar, session switch; other repos' command/event handlers) as WASM
guest exports. The host drives one guest export per call; a guest that needs a
dynamic callback (a registered CommandSpec/ModeSpec handler, a key hook, an
event handler) exposes it as an export reached via this surface. Same
scratch-and-call shape as core `on_change`.

| Surface | Signature | Meaning |
|---|---|---|
| `Context::call(id, entry, payload)` | `-> Result<String>` | host writes `payload` into the guest's `scratch()`, calls the guest export `entry`, drains ops emitted during the call via `take_ops`, reads a result string back |
| `entry` (return form) | `(ptr,len) -> (ret_ptr,ret_len)` | guest returns a pointer into its own memory the host validates and reads |
| `entry` (void form) | `(ptr,len) -> ()` + `ctx_return` | guest delivers its result through the `host.ctx_return` host function |
| `ctx_return` | `(ptr,len) -> ()` | guest-side host function recording its result string for the current call (validated; bad input traps) |

Only string/JSON flows across the boundary ([[principle:functional-core]]): the
guest reads an immutable payload snapshot and returns a string action; it never
receives `&mut` host state. During a call `current_id` is set, so the guest's
`ctx_set` / `register_*` / buffered ops attribute to that plugin. A trap during a
call discards the call's buffered ops (temporal): no residue.

**Fuel metering (wasmtime 'fuel' convention, wasmtime 1 — wired, not aspirational).**
Every wasm invocation (mount, `on_change`, and `Context::call`) tops up a fresh
per-call fuel budget before running; a guest that spins forever exhausts it and
traps with `Trap::OutOfFuel` instead of hanging the host. Draw budget 200k /
call budget 2M instructions as claimed above (kernel default call budget
`CALL_FUEL_BUDGET = 2_000_000`).

**Module under one `host` module.** `ctx_return` lives with the rest of the host
functions; the dispatch entry exports are plain guest exports. Builtins use the
same machine (`Context::call`) as user extensions ([[principle:no-privileged-path]]).

**Tests:** `crates/cordis/tests/host_call.rs` proves (a) payload in / distinct
result out, (b) guest->host ops emitted during a call drained via `take_ops`,
(c) a runaway guest overrunning the budget traps `OutOfFuel` with no panic and
no state residue (plugin stays mounted; a subsequent call recovers via a fresh
budget).

## Design invariants (locked)

1. **Temporal** — kernel owns inverses; guest never trusted to revert.
2. **Spatial** — `ctx_read` declares; host `set` notifies exactly those readers.
3. **Two-tier** — guest writes are effects (no notify); host `set` is the committed write.
4. **Functional-core** — guest reads immutable snapshot, returns actions/JSON; never holds `&mut` host state.
5. **No privileged path** — builtins use the same WASM ABI as user extensions.

## Migration order

1. Grow cordis-rs with function sets 2-5 (one place, once).
2. pi-rs (closest semantics) — includes pi-harness JS -> WASM.
3. ekko (wasm crate half-built, 4 consumers).
4. tomoe (greenfield, largest).


## Architecture resolution: extension boundary vs host mechanism (locked)

Investigation of every consumer confirmed the repos' kernels play TWO distinct roles
that must be separated before migration:

1. **Config + extension/scripting surface** — this is what migrates to cordis-rs WASM:
   every Lua evaluator (`pi.kernel` Lua, `ekko-lua`, `tomoe.lua`) and every extension
   bridge becomes a WASM guest on cordis-rs. This is where all Lua is deleted and where
   one shared extension ABI lives. Config ships as a compiled `.wasm` loaded at startup.
2. **Host-owned native resource composition** — session managers, VMs, process trees,
   terminal render trees are typed native handles (`Arc<Mutex<...>>`, `Epoch`,
   `SessionContext`, `Arc<dyn Component>`). A WASM guest CANNOT hold or pass these
   (WASM values are numbers/Strings; a String cannot serialize an `Arc<Mutex<...>>`).
   These are HOST mechanism, not an extension boundary. The host keeps them native and
   exposes only *string-keyed descriptors* into cordis-rs; extensions reach native
   services through dedicated ABI host functions (the "grow once" function sets), never
   by placing native handles inside the WASM context.

Consequences (locked):
- `[[principle:functional-core]]` holds at the WASM boundary: extensions read an immutable
  snapshot (String/JSON data + string-keyed service handles), return actions, never hold
  `&mut` host state.
- `[[principle:no-privileged-path]]` holds: builtins use the same public WASM ABI as user
  extensions.
- The bundling of a native resource kernel (pi-rs-kernel) is NOT recreated as WASM guests;
  its spatiotemporal *semantics* (inverses, declared reads, committed set notifications)
  are the same paradigm cordis-rs implements for config/extensions. Native resource
  composition stays host-side.
- Migration scope per repo = config-as-WASM + Lua surface removal + extension bridge → WASM
  + the host-function sets below used for reaching native services. It is NOT "orphan every
  native handle into a WASM guest."
