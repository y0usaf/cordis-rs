//! Spatiotemporal composability kernel with a WASM plugin boundary.
//!
//! - **Temporal**: the kernel records an inverse for every write a plugin
//!   makes (the previous value, or absence) and restores them in reverse on
//!   unmount — so the context returns to its pre-mount state with no residue.
//!   Revertibility is kernel-owned, never trusted to the guest.
//! - **Spatial**: a plugin declares the keys it reads (`ctx_read`); a committed
//!   [`Context::set`] notifies exactly the plugins that declared that key via
//!   their `on_change` export.
//!
//! Two-tier writes: guest `ctx_set`/`ctx_remove` are *effects* (no
//! notification); host [`Context::set`] is the *committed* write that notifies
//! readers. The context is host-owned; plugins touch it only through
//! access-only host functions.
//!
//! # Extension surfaces (following `docs/abi.md`)
//!
//! Beyond core set 1 (`ctx_set`/`ctx_remove`/`ctx_read`) the kernel grows the
//! host function sets extensions need, all on the one `host` module:
//!
//! - **Set 2 — extension registration (ekko).** `register_command`,
//!   `register_surface`, `register_keybinding`, `register_mode`,
//!   `register_overlay`, `register_theme`, `register_spinner`,
//!   `register_session_grouper`, `register_session_namer`, `subscribe`,
//!   `register_action_interpreter`. Each validates its pointer/len string
//!   arguments (a trap on bad input, never a panic) and records a
//!   `(name, kind, descriptors)` unit in a per-plugin registry: the name plus
//!   every validated descriptor after it (a keybinding's mode, a surface's
//!   dock/priority/size, ...) survives, so the host can reconstruct the full
//!   registration. The registry is kernel-owned: on unmount it is cleared
//!   wholesale, so registrations are reverted like any other effect.
//!
//! - **Set 3 — draw ops (ekko) and set 5 — compositor ops (tomoe).** Data-only
//!   ops. Each named function validates its string arguments and appends a
//!   `(kind, args)` record to a per-plugin op buffer. The host drains the
//!   buffer after a *clean* guest return (`Context::take_ops`); a trap leaves
//!   nothing to drain. Two accessors stay value-returning: `size() -> (w,h)`.
//!
//! Functional-core holds at the WASM boundary: a guest never receives a
//! `&mut` handle to host state — it requests registrations/ops that the host
//! applies afterwards. Builtins use the same public WASM ABI.
//!
//! # Host->guest dispatch (dynamic callbacks)
//!
//! [`Context::call`] restores the live-callback surface that Lua used to own
//! (which-key's key hook, modes, status bar, session switch; other repos'
//! command/event handlers) as WASM guest exports. The host writes a payload
//! string into the guest's `scratch()`, calls an export
//! `(ptr, len) -> (ret_ptr, ret_len)` **or** `(ptr, len) -> ()` + `ctx_return`,
//! drains the ops the guest emitted during the call, and reads the result
//! string back. Fuel-metered by [`CALL_FUEL_BUDGET`] so a runaway guest traps
//! instead of hanging the host. The `scratch()` export may be either
//! `() -> (ptr, cap)` (hand-written `.wat` guests) or `() -> ptr` (Rust-authored
//! guests, since rustc lowers a `(i32, i32)` return to C sret on
//! `wasm32-unknown-unknown`); see [`Context::scratch_region`].

use std::collections::HashMap;
use wasmtime::*;

/// Tight registration wrapper: reads N validation string pointer/len pairs
/// (name then descriptors), then records a [`Registration`] unit. The first
/// field is the unit's name; every remaining field is kept as a descriptor
/// (e.g. a keybinding's mode, a surface's dock/priority/size). All fields are
/// validated so bad input traps, and the full descriptor set survives so the
/// host can reconstruct mode-scoped bindings and surface geometry instead of
/// discarding it.
macro_rules! register_fn {
    ($linker:ident, $wname:literal, $kind:literal $(, ($p:ident, $l:ident))*) => {
        $linker.func_wrap(
            "host",
            $wname,
            |mut caller: Caller<'_, State> $(, $p: i32, $l: i32)*| -> Result<(), wasmtime::Error> {
                let fields: Vec<String> = vec![$(read_str(&mut caller, $p, $l)?),*];
                let name = fields[0].clone();
                let descriptors = fields[1..].to_vec();
                register_unit(&mut caller, $kind, name, descriptors);
                Ok(())
            },
        )?
    };
}

/// Op wrapper: reads N validation string pointer/len pairs and buffers one
/// `(kind, args)` op for the current plugin. The op kind is the WASM function
/// name (e.g. `fill_rect`, `tomoe.bind`, `tomoe.window.id`), so ops are
/// self-describing for the host to apply.
macro_rules! op {
    ($linker:ident, $wname:literal) => {
        $linker.func_wrap(
            "host",
            $wname,
            |mut caller: Caller<'_, State>| -> Result<(), wasmtime::Error> {
                buffer_op(&mut caller, $wname, Vec::new());
                Ok(())
            },
        )?
    };
    ($linker:ident, $wname:literal $(, ($p:ident, $l:ident))*) => {
        $linker.func_wrap(
            "host",
            $wname,
            |mut caller: Caller<'_, State> $(, $p: i32, $l: i32)*| -> Result<(), wasmtime::Error> {
                let args: Vec<String> = vec![$(read_str(&mut caller, $p, $l)?),*];
                buffer_op(&mut caller, $wname, args);
                Ok(())
            },
        )?
    };
}

type Op = (String, Vec<String>);
/// An extension unit a guest registered: `(name, kind, descriptors)`. The
/// first two are the coherence unit the host reverts on unmount; `descriptors`
/// carries the validated arguments after the name (a keybinding's mode, a
/// surface's dock/priority/size, ...) so the host can reconstruct the full
/// registration instead of discarding them.
type Registration = (String, String, Vec<String>);

/// Per-invocation fuel budget (wasmtime 'fuel' convention, wasmtime 1): every
/// wasm call — mount, `on_change`, and [`Context::call`] — tops up this many
/// fuel units before running. A guest that loops forever exhausts the budget
/// and traps with `Trap::OutOfFuel` instead of hanging the host. Matches the
/// "call budget 2M instructions" figure in `docs/abi.md`.
const CALL_FUEL_BUDGET: u64 = 2_000_000;

struct Plugin {
    instance: Instance,
    reads: Vec<String>,
    /// key -> previous value (`None` = absent before this plugin wrote it).
    inverses: HashMap<String, Option<String>>,
    /// Extension units this guest registered: `(name, kind, descriptors)`.
    /// Reverted by clearing the whole vec on unmount — kernel-owned reversion.
    registered: Vec<Registration>,
    /// Ops (draw + compositor) buffered during the current guest call. The
    /// host drains these after a clean return; a trap discards them.
    ops: Vec<Op>,
    /// Result string delivered during the current host->guest [`Context::call`]
    /// via the `ctx_return` host function (the void-export form). Cleared at
    /// the start of every call so no stale value leaks across calls.
    result: Option<String>,
    mounted: bool,
}

#[derive(Default)]
struct State {
    values: HashMap<String, String>,
    readers: HashMap<String, Vec<usize>>,
    plugins: Vec<Plugin>,
    /// The plugin currently executing (during mount or on_change), so guest
    /// writes and reads are attributed to it.
    current_id: Option<usize>,
}

/// Host-owned context. Plugins are WASM instances; the kernel tracks their
/// lifecycle, inverses, and the reactive (coeffect) graph.
pub struct Context {
    engine: Engine,
    store: Store<State>,
}

impl Context {
    pub fn new() -> Self {
        // Fuel metering (wasmtime 'fuel' convention): an untrusted guest that
        // loops forever exhausts its per-invocation budget and traps rather
        // than hanging the host. `consume_fuel` must be enabled on the engine
        // config, and every wasm invocation (mount, on_change, call) tops up a
        // fresh budget before running (docs/abi.md "call budget 2M").
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).expect("engine with fuel consumption");
        let mut store = Store::new(&engine, State::default());
        store
            .set_fuel(CALL_FUEL_BUDGET)
            .expect("initial fuel budget");
        Context { engine, store }
    }

    /// Instantiate a WASM plugin, run its `mount` export, and register its
    /// declared reads. On any failure, rolls back every write and read the
    /// plugin made, leaving the context unchanged. Returns the plugin id.
    /// Owned by the host afterwards: `take_ops` drains any ops the guest
    /// buffered during `mount`, `registrations` exposes registered units.
    pub fn mount(&mut self, wasm: &[u8]) -> Result<usize> {
        let module = Module::new(&self.engine, wasm)?;
        let mut linker = Linker::new(&self.engine);
        Self::add_host_funcs(&mut linker)?;
        let instance = linker.instantiate(&mut self.store, &module)?;

        let id = self.store.data().plugins.len();
        self.store.data_mut().plugins.push(Plugin {
            instance,
            reads: Vec::new(),
            inverses: HashMap::new(),
            registered: Vec::new(),
            ops: Vec::new(),
            result: None,
            mounted: true,
        });
        self.store.data_mut().current_id = Some(id);
        self.store.set_fuel(CALL_FUEL_BUDGET)?;

        let instance = self.store.data().plugins[id].instance;
        let mount = instance
            .get_typed_func::<(), ()>(&mut self.store, "mount")
            .and_then(|f| f.call(&mut self.store, ()));

        self.store.data_mut().current_id = None;

        // Register declared reads (deduped).
        let reads = self.store.data().plugins[id].reads.clone();
        for key in &reads {
            let v = self
                .store
                .data_mut()
                .readers
                .entry(key.clone())
                .or_default();
            if !v.contains(&id) {
                v.push(id);
            }
        }

        if let Err(e) = mount {
            self.rollback(id);
            return Err(e);
        }
        Ok(id)
    }

    /// Restore every write the plugin made (kernel-side inverse replay),
    /// unregister its reads, and drop its registrations + pending ops.
    /// Idempotent.
    pub fn unmount(&mut self, id: usize) -> Result<()> {
        if id >= self.store.data().plugins.len() {
            return Err(wasmtime::Error::msg("invalid plugin id"));
        }
        if !self.store.data().plugins[id].mounted {
            return Ok(());
        }
        self.rollback(id);
        Ok(())
    }

    /// The single committed write path: store the value and notify exactly the
    /// plugins that declared `key`.
    pub fn set(&mut self, key: &str, val: &str) -> Result<()> {
        self.store
            .data_mut()
            .values
            .insert(key.to_string(), val.to_string());
        let readers = self
            .store
            .data()
            .readers
            .get(key)
            .cloned()
            .unwrap_or_default();
        for id in readers {
            self.notify(id, key)?;
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.store.data().values.get(key).cloned()
    }

    pub fn has(&self, key: &str) -> bool {
        self.store.data().values.contains_key(key)
    }

    /// Kernel-owned view of the extension units this guest registered (if
    /// any), as `(name, kind, descriptors)`. Bounds-checked only, so after
    /// unmount this returns the empty registry — the observable proof that
    /// registrations were reverted.
    pub fn registrations(&self, id: usize) -> Result<Vec<Registration>> {
        if id >= self.store.data().plugins.len() {
            return Err(wasmtime::Error::msg("invalid plugin id"));
        }
        Ok(self.store.data().plugins[id].registered.clone())
    }

    /// Drain the ops the guest buffered during its last clean call. A mounted
    /// plugin with an empty buffer returns `vec![]`; a failed call leaves
    /// nothing here (a trap discards it).
    pub fn take_ops(&mut self, id: usize) -> Result<Vec<Op>> {
        self.assert_mounted(id)?;
        Ok(std::mem::take(&mut self.store.data_mut().plugins[id].ops))
    }

    fn assert_mounted(&self, id: usize) -> Result<()> {
        if id >= self.store.data().plugins.len() {
            return Err(wasmtime::Error::msg("invalid plugin id"));
        }
        if !self.store.data().plugins[id].mounted {
            return Err(wasmtime::Error::msg("plugin is unmounted"));
        }
        Ok(())
    }

    /// Restore the plugin's inverses (reverse of write order), unregister its
    /// reads, and clear its registrations + pending ops. Marks unmounted.
    fn rollback(&mut self, id: usize) {
        let inverses = std::mem::take(&mut self.store.data_mut().plugins[id].inverses);
        for (key, prev) in inverses {
            match prev {
                Some(v) => {
                    self.store.data_mut().values.insert(key, v);
                }
                None => {
                    self.store.data_mut().values.remove(&key);
                }
            }
        }
        let reads = self.store.data().plugins[id].reads.clone();
        for key in &reads {
            if let Some(v) = self.store.data_mut().readers.get_mut(key) {
                v.retain(|&x| x != id);
            }
        }
        let plugin = &mut self.store.data_mut().plugins[id];
        plugin.registered.clear();
        plugin.ops.clear();
        plugin.mounted = false;
    }

    /// Resolve the guest's scratch buffer for host->guest payload/notify
    /// writes. Backward-compatible with two guest shapes:
    /// - `scratch() -> (ptr, cap)` — the classic multi-value form (hand-written
    ///   `.wat` guests). The returned pointer is the buffer base and `cap` is
    ///   its byte capacity.
    /// - `scratch() -> ptr` — the single-value form a Rust-authored guest can
    ///   actually emit (rustc cannot lower a `(i32, i32)` return on
    ///   `wasm32-unknown-unknown` without C sret). Here the guest reserves a
    ///   fixed scratch arena and the host derives a 64 KiB capacity; the
    ///   buffer base is `mem[ptr]`.
    ///
    /// Returns `(ptr, cap)`; validates both are non-negative.
    fn scratch_region(&mut self, id: usize) -> Result<(i32, i32)> {
        const DEFAULT_SINGLE_CAP: i32 = 64 * 1024;
        self.assert_mounted(id)?;
        let instance = self.store.data().plugins[id].instance;
        if let Ok(scratch) = instance.get_typed_func::<(), (i32, i32)>(&mut self.store, "scratch") {
            let (ptr, cap) = scratch.call(&mut self.store, ())?;
            if ptr < 0 || cap < 0 {
                return Err(wasmtime::Error::msg(
                    "guest scratch pointer/capacity negative",
                ));
            }
            return Ok((ptr, cap));
        }
        if let Ok(scratch) = instance.get_typed_func::<(), i32>(&mut self.store, "scratch") {
            let ptr = scratch.call(&mut self.store, ())?;
            if ptr < 0 {
                return Err(wasmtime::Error::msg("guest scratch pointer negative"));
            }
            return Ok((ptr, DEFAULT_SINGLE_CAP));
        }
        Err(wasmtime::Error::msg(
            "guest must export scratch() -> (i32, i32) or scratch() -> i32",
        ))
    }

    /// Deliver a changed key to a reader: write it into the guest's reserved
    /// scratch buffer and invoke its `on_change`.
    fn notify(&mut self, id: usize, key: &str) -> Result<()> {
        let (ptr, cap) = self.scratch_region(id)?;
        if key.len() > cap as usize {
            return Err(wasmtime::Error::msg("guest scratch buffer too small"));
        }
        let instance = self.store.data().plugins[id].instance;
        let mem = instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest must export memory"))?;
        mem.write(&mut self.store, ptr as usize, key.as_bytes())?;

        self.store.data_mut().current_id = Some(id);
        self.store.set_fuel(CALL_FUEL_BUDGET)?;
        let on_change = instance.get_typed_func::<(i32, i32), ()>(&mut self.store, "on_change")?;
        let result = on_change.call(&mut self.store, (ptr, key.len() as i32));
        self.store.data_mut().current_id = None;
        if result.is_err() {
            self.store.data_mut().plugins[id].ops.clear();
        }
        result
    }

    /// Dynamic host->guest dispatch: drive an arbitrary guest export as a live
    /// callback. Restores the dynamic-callback surface that Lua used to own
    /// (which-key's key hook, modes, status bar, session switch; other repos'
    /// command/event handlers) as a WASM guest export.
    ///
    /// Protocol (the same scratch-and-call shape as [`Context::notify`]):
    /// 1. `current_id` is set to `id` for the duration of the call, so any
    ///    `ctx_set`/`register_*`/op the guest emits is attributed to it and
    ///    an ops buffer is drained afterwards.
    /// 2. The host writes `payload` into the guest's `scratch()` buffer,
    /// 3. calls the guest export named `entry`, whose signature must be
    ///    `(ptr, len) -> (ret_ptr, ret_len)` (the guest returns a pointer into
    ///    its own memory that the host reads back) **or** `(ptr, len) -> ()`
    ///    (the guest delivers its result via the `host.ctx_return` function).
    /// 4. Any ops the guest buffered during the call are left in its buffer —
    ///    the host drains them with [`Context::take_ops`], exactly like the
    ///    mount path. A trap discards them.
    ///
    /// Fuel: a fresh [`CALL_FUEL_BUDGET`] is set before the call, so a guest
    /// that spins forever exhausts it and traps (`Trap::OutOfFuel`) instead of
    /// hanging the host. Failed calls leave no state residue (ops discarded,
    /// `current_id` cleared); the plugin stays mounted for further calls.
    ///
    /// # Errors
    /// Traps (never panics) on: invalid id, missing `scratch`/`memory`, an
    /// unknown or wrongly-typed `entry` export, a payload larger than the
    /// scratch capacity, or a guest trap (fuel exhaustion included).
    pub fn call(&mut self, id: usize, entry: &str, payload: &str) -> Result<String> {
        self.assert_mounted(id)?;
        // Fresh call: clear any op buffer and stale result so this call's
        // ops/results are exactly what take_ops / the return value observe.
        let plugins = &mut self.store.data_mut().plugins;
        plugins[id].ops.clear();
        plugins[id].result = None;

        let instance = self.store.data().plugins[id].instance;
        let (ptr, cap) = self.scratch_region(id)?;
        if payload.len() > cap as usize {
            return Err(wasmtime::Error::msg(
                "guest scratch buffer too small for payload",
            ));
        }
        let mem = instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest must export memory"))?;
        if !payload.is_empty() {
            mem.write(&mut self.store, ptr as usize, payload.as_bytes())?;
        }

        let func = instance
            .get_func(&mut self.store, entry)
            .ok_or_else(|| wasmtime::Error::msg(format!("guest does not export `{entry}`")))?;

        self.store.data_mut().current_id = Some(id);
        self.store.set_fuel(CALL_FUEL_BUDGET)?;

        // Dispatch on the export's signature: two-result (guest returns
        // (ret_ptr,ret_len)) or void (guest uses ctx_return).
        let outcome: Result<(i32, i32)> = (|| {
            if let Ok(f) = func.typed::<(i32, i32), (i32, i32)>(&mut self.store) {
                f.call(&mut self.store, (ptr, payload.len() as i32))
            } else if let Ok(f) = func.typed::<(i32, i32), ()>(&mut self.store) {
                f.call(&mut self.store, (ptr, payload.len() as i32))?;
                Ok((-1, -1)) // sentinel: void form, result came via ctx_return
            } else {
                Err(wasmtime::Error::msg(format!(
                    "guest export `{entry}` must be (i32,i32) -> (i32,i32) or (i32,i32) -> ()"
                )))
            }
        })();

        let recorded = self.store.data().plugins[id].result.clone();
        self.store.data_mut().current_id = None;

        match outcome {
            Err(e) => {
                // A trap (fuel exhaustion included) leaves nothing to drain.
                self.store.data_mut().plugins[id].ops.clear();
                Err(e)
            }
            // Two-return form: read the result the guest pointed into memory.
            Ok((rp, rl)) if rp >= 0 => read_guest_str(&mut self.store, &mem, rp, rl),
            // Void form: the result is what ctx_return recorded.
            Ok(_) => Ok(recorded.unwrap_or_default()),
        }
    }

    fn add_host_funcs(linker: &mut Linker<State>) -> Result<()> {
        // ===== Function set 1 — core kernel =====
        linker.func_wrap(
            "host",
            "ctx_set",
            |mut caller: Caller<'_, State>,
             kp: i32,
             kl: i32,
             vp: i32,
             vl: i32|
             -> Result<(), wasmtime::Error> {
                let key = read_str(&mut caller, kp, kl)?;
                let val = read_str(&mut caller, vp, vl)?;
                record_inverse(&mut caller, &key);
                caller.data_mut().values.insert(key, val);
                Ok(())
            },
        )?;
        linker.func_wrap(
            "host",
            "ctx_remove",
            |mut caller: Caller<'_, State>, kp: i32, kl: i32| -> Result<(), wasmtime::Error> {
                let key = read_str(&mut caller, kp, kl)?;
                record_inverse(&mut caller, &key);
                caller.data_mut().values.remove(&key);
                Ok(())
            },
        )?;
        linker.func_wrap(
            "host",
            "ctx_read",
            |mut caller: Caller<'_, State>, kp: i32, kl: i32| -> Result<(), wasmtime::Error> {
                let key = read_str(&mut caller, kp, kl)?;
                if let Some(id) = caller.data().current_id {
                    let reads = &mut caller.data_mut().plugins[id].reads;
                    if !reads.iter().any(|r| r == &key) {
                        reads.push(key);
                    }
                }
                Ok(())
            },
        )?;
        // `ctx_return`: the guest-side counterpart of the host->guest
        // dispatch. During a [`Context::call`] the guest delivers its result
        // string through this host function (the void-export form). The string
        // is validated (bad input traps, never panics) and recorded against the
        // current plugin for the host to read back after the clean return.
        linker.func_wrap(
            "host",
            "ctx_return",
            |mut caller: Caller<'_, State>, rp: i32, rl: i32| -> Result<(), wasmtime::Error> {
                let result = read_str(&mut caller, rp, rl)?;
                if let Some(id) = caller.data().current_id {
                    caller.data_mut().plugins[id].result = Some(result);
                }
                Ok(())
            },
        )?;

        // ===== Function set 2 — extension registration (ekko) =====
        register_fn!(linker, "register_command", "command", (np, nl), (dp, dl));
        register_fn!(
            linker,
            "register_keybinding",
            "keybinding",
            (cp, cl),
            (mp, ml),
            (dp, dl),
            (hp, hl)
        );
        register_fn!(
            linker,
            "register_mode",
            "mode",
            (np, nl),
            (kp, kl),
            (ip, il),
            (rp, rl)
        );
        // `register_overlay` carries its description and mode attachment
        // alongside the render/key/init handlers, so a leader-attached
        // (session-list) overlay can be reconstructed by the host. Descriptor
        // shape: [description, render, key, init, attach_mode].
        linker.func_wrap(
            "host",
            "register_overlay",
            |mut caller: Caller<'_, State>,
             np: i32,
             nl: i32,
             dp: i32,
             dl: i32,
             rp: i32,
             rl: i32,
             hkp: i32,
             hkl: i32,
             ip: i32,
             il: i32,
             ap: i32,
             al: i32|
             -> Result<(), wasmtime::Error> {
                let name = read_str(&mut caller, np, nl)?;
                let desc = read_str(&mut caller, dp, dl)?;
                let render = read_str(&mut caller, rp, rl)?;
                let key = read_str(&mut caller, hkp, hkl)?;
                let init = read_str(&mut caller, ip, il)?;
                let attach = read_str(&mut caller, ap, al)?;
                if let Some(id) = caller.data().current_id {
                    caller.data_mut().plugins[id].registered.push((
                        name,
                        "overlay".to_string(),
                        vec![desc, render, key, init, attach],
                    ));
                }
                Ok(())
            },
        )?;
        register_fn!(
            linker,
            "register_theme",
            "theme",
            (np, nl),
            (dp, dl),
            (hp, hl)
        );
        register_fn!(linker, "register_spinner", "spinner", (np, nl), (hp, hl));
        register_fn!(
            linker,
            "register_session_grouper",
            "session_grouper",
            (np, nl),
            (hp, hl)
        );
        register_fn!(
            linker,
            "register_session_namer",
            "session_namer",
            (np, nl),
            (hp, hl)
        );
        register_fn!(linker, "subscribe", "subscription", (ep, el), (hp, hl));
        register_fn!(
            linker,
            "register_action_interpreter",
            "action_interpreter",
            (np, nl),
            (dp, dl),
            (hp, hl)
        );
        // register_surface carries three scalar descriptors (dock, priority,
        // size) alongside its name.
        linker.func_wrap(
            "host",
            "register_surface",
            |mut caller: Caller<'_, State>,
             np: i32,
             nl: i32,
             dock: i32,
             priority: i32,
             size: i32|
             -> Result<(), wasmtime::Error> {
                if dock < 0 || priority < 0 || size < 0 {
                    return Err(wasmtime::Error::msg("invalid surface descriptor"));
                }
                let name = read_str(&mut caller, np, nl)?;
                let descriptors = vec![dock.to_string(), priority.to_string(), size.to_string()];
                if let Some(id) = caller.data().current_id {
                    caller.data_mut().plugins[id].registered.push((
                        name,
                        "surface".to_string(),
                        descriptors,
                    ));
                }
                Ok(())
            },
        )?;

        // ===== Function set 3 — draw ops (ekko) =====
        // `size()` is the one value-returning draw accessor (canvas size).
        linker.func_wrap("host", "size", |_: Caller<'_, State>| -> (i32, i32) {
            (80, 24)
        })?;
        // Data-only draw ops: validated string params, buffered per call.
        op!(
            linker,
            "fill_rect",
            (xp, xl),
            (yp, yl),
            (wp, wl),
            (hp, hl),
            (cp_, cl_)
        );
        op!(linker, "set_cell", (xp, xl), (yp, yl), (cp_, cl_));
        op!(linker, "put_text", (xp, xl), (yp, yl), (tp, tl), (cpp, cpl));
        op!(
            linker,
            "put_text_bold",
            (xp, xl),
            (yp, yl),
            (tp, tl),
            (cpp, cpl)
        );
        op!(
            linker,
            "put_text_styled",
            (xp, xl),
            (yp, yl),
            (tp, tl),
            (sp, sl)
        );
        op!(
            linker,
            "draw_box",
            (xp, xl),
            (yp, yl),
            (wp, wl),
            (hp, hl),
            (cp_, cl_)
        );
        op!(
            linker,
            "render_scrollbar",
            (xp, xl),
            (yp, yl),
            (wp, wl),
            (hp, hl),
            (op_, ol),
            (pp, pl)
        );

        // ===== Function set 5 — compositor ops (tomoe) =====
        // Top-level `tomoe.*`.
        op!(linker, "tomoe.settings", (tp, tl));
        op!(linker, "tomoe.bind", (cp, cl), (ap, al), (dp, dl));
        op!(linker, "tomoe.spawn", (cp, cl));
        op!(linker, "tomoe.clear_focus");
        op!(linker, "tomoe.quit");
        op!(linker, "tomoe.windows");
        op!(linker, "tomoe.window", (ip, il));
        op!(linker, "tomoe.rule", (sp, sl));
        op!(linker, "tomoe.rules_for", (wp, wl));
        op!(linker, "tomoe.focused_window");
        op!(linker, "tomoe.usable_area", (ip, il));
        op!(linker, "tomoe.outputs");
        op!(linker, "tomoe.view");
        op!(linker, "tomoe.set_view", (tp, tl));
        op!(linker, "tomoe.pointer");
        op!(linker, "tomoe.grab_pointer", (mp, ml), (rp, rl));
        op!(linker, "tomoe.ungrab_pointer");
        op!(linker, "tomoe.on_reload", (np, nl), (sp, sl), (rp, rl));
        // `tomoe.process`.
        op!(linker, "tomoe.process.once", (ip, il), (op_, ol));
        op!(linker, "tomoe.process.service", (ip, il), (op_, ol));
        op!(linker, "tomoe.process.spawn", (op_, ol));
        // `tomoe.ipc`.
        op!(linker, "tomoe.ipc.serve", (mp, ml), (hp, hl));
        op!(linker, "tomoe.ipc.broadcast", (ep, el), (pp, pl));
        // `tomoe.ui`.
        op!(linker, "tomoe.ui.confirm", (sp, sl));
        op!(linker, "tomoe.ui.menu", (sp, sl));
        op!(linker, "tomoe.ui.toast", (sp, sl));
        op!(linker, "tomoe.ui.sheet", (sp, sl));
        // Event hooks.
        op!(linker, "tomoe.on_window_open", (fp, fl));
        op!(linker, "tomoe.on_window_close", (fp, fl));
        op!(linker, "tomoe.on_focus_change", (fp, fl));
        op!(linker, "tomoe.on_outputs_changed", (fp, fl));
        op!(linker, "tomoe.on_pointer_button", (fp, fl));
        op!(linker, "tomoe.on_pointer_axis", (fp, fl));
        op!(linker, "tomoe.on_pointer_enter", (fp, fl));
        op!(linker, "tomoe.on_pointer_leave", (fp, fl));
        op!(linker, "tomoe.on_window_request", (fp, fl));
        op!(linker, "tomoe.on_screencast_request", (fp, fl));
        // Window methods.
        op!(linker, "tomoe.window.id");
        op!(linker, "tomoe.window.app_id");
        op!(linker, "tomoe.window.title");
        op!(linker, "tomoe.window.is_mapped");
        op!(linker, "tomoe.window.is_focused");
        op!(linker, "tomoe.window.is_fullscreen");
        op!(linker, "tomoe.window.is_maximized");
        op!(linker, "tomoe.window.geometry");
        op!(linker, "tomoe.window.set_properties", (sp, sl));
        op!(linker, "tomoe.window.show");
        op!(linker, "tomoe.window.hide");
        op!(linker, "tomoe.window.focus");
        op!(linker, "tomoe.window.raise");
        op!(linker, "tomoe.window.set_fullscreen");
        op!(linker, "tomoe.window.set_maximized");
        op!(linker, "tomoe.window.close");

        Ok(())
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Record the inverse for a guest effect (`ctx_set`/`ctx_remove`) if a plugin
/// is currently executing so the kernel can restore it on unmount.
fn record_inverse(caller: &mut Caller<'_, State>, key: &str) {
    if let Some(id) = caller.data().current_id {
        let prev = caller.data().values.get(key).cloned();
        let inverses = &mut caller.data_mut().plugins[id].inverses;
        inverses.entry(key.to_string()).or_insert(prev);
    }
}

/// Register an extension unit against the current plugin. No plugin executing
/// means this is a top-level host call, so nothing is attributed.
fn register_unit(
    caller: &mut Caller<'_, State>,
    kind: &str,
    name: String,
    descriptors: Vec<String>,
) {
    if let Some(id) = caller.data().current_id {
        caller.data_mut().plugins[id]
            .registered
            .push((name, kind.to_string(), descriptors));
    }
}

/// Buffer one validated data op for the current plugin; the host drains it
/// after a clean guest return.
fn buffer_op(caller: &mut Caller<'_, State>, kind: &str, args: Vec<String>) {
    if let Some(id) = caller.data().current_id {
        caller.data_mut().plugins[id]
            .ops
            .push((kind.to_string(), args));
    }
}

/// Read a guest string with full validation: no negative ptr/len, no
/// out-of-bounds read, no non-UTF-8. Returns a trap (not a panic) on bad input.
fn read_str(caller: &mut Caller<'_, State>, ptr: i32, len: i32) -> Result<String, wasmtime::Error> {
    if ptr < 0 || len < 0 {
        return Err(wasmtime::Error::msg("invalid guest pointer/length"));
    }
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest must export memory"))?;
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf)
        .map_err(|_| wasmtime::Error::msg("out-of-bounds guest read"))?;
    String::from_utf8(buf).map_err(|_| wasmtime::Error::msg("guest string is not utf-8"))
}

/// Read a guest string back from a [`Memory`] handle (used by
/// [`Context::call`] to collect a result the guest returned as `(ret_ptr,
/// ret_len)`). Same validation contract as [`read_str`] — pointer/length
/// checked, OOB and non-UTF-8 trap, never panic.
fn read_guest_str(
    store: &mut Store<State>,
    mem: &Memory,
    ptr: i32,
    len: i32,
) -> Result<String, wasmtime::Error> {
    if ptr < 0 || len < 0 {
        return Err(wasmtime::Error::msg("invalid guest pointer/length"));
    }
    let mut buf = vec![0u8; len as usize];
    mem.read(store, ptr as usize, &mut buf)
        .map_err(|_| wasmtime::Error::msg("out-of-bounds guest read"))?;
    String::from_utf8(buf).map_err(|_| wasmtime::Error::msg("guest string is not utf-8"))
}
