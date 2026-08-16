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

use std::collections::HashMap;
use wasmtime::*;

struct Plugin {
    instance: Instance,
    reads: Vec<String>,
    /// key -> previous value (`None` = absent before this plugin wrote it).
    inverses: HashMap<String, Option<String>>,
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
        let engine = Engine::default();
        let store = Store::new(&engine, State::default());
        Context { engine, store }
    }

    /// Instantiate a WASM plugin, run its `mount` export, and register its
    /// declared reads. On any failure, rolls back every write and read the
    /// plugin made, leaving the context unchanged. Returns the plugin id.
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
            mounted: true,
        });
        self.store.data_mut().current_id = Some(id);

        let mount = self
            .store
            .data()
            .plugins[id]
            .instance
            .clone()
            .get_typed_func::<(), ()>(&mut self.store, "mount")
            .and_then(|f| f.call(&mut self.store, ()));

        self.store.data_mut().current_id = None;

        // Register declared reads (deduped).
        let reads = self.store.data().plugins[id].reads.clone();
        for key in &reads {
            let v = self.store.data_mut().readers.entry(key.clone()).or_default();
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

    /// Restore every write the plugin made (kernel-side inverse replay) and
    /// unregister its reads. Idempotent.
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

    /// Restore the plugin's inverses (reverse of write order) and unregister
    /// its reads. Marks the plugin unmounted.
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
        self.store.data_mut().plugins[id].mounted = false;
    }

    /// Deliver a changed key to a reader: write it into the guest's reserved
    /// scratch buffer and invoke its `on_change`.
    fn notify(&mut self, id: usize, key: &str) -> Result<()> {
        let instance = self.store.data().plugins[id].instance.clone();
        let scratch = instance.get_typed_func::<(), (i32, i32)>(&mut self.store, "scratch")?;
        let (ptr, cap) = scratch.call(&mut self.store, ())?;
        if ptr < 0 || cap < 0 || key.len() > cap as usize {
            return Err(wasmtime::Error::msg("guest scratch buffer too small"));
        }
        let mem = instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest must export memory"))?;
        mem.write(&mut self.store, ptr as usize, key.as_bytes())?;

        self.store.data_mut().current_id = Some(id);
        let on_change = instance.get_typed_func::<(i32, i32), ()>(&mut self.store, "on_change")?;
        let result = on_change.call(&mut self.store, (ptr, key.len() as i32));
        self.store.data_mut().current_id = None;
        result
    }

    fn add_host_funcs(linker: &mut Linker<State>) -> Result<()> {
        linker.func_wrap(
            "host",
            "ctx_set",
            |mut caller: Caller<'_, State>, kp: i32, kl: i32, vp: i32, vl: i32| -> Result<(), wasmtime::Error> {
                let key = read_str(&mut caller, kp, kl)?;
                let val = read_str(&mut caller, vp, vl)?;
                let id = caller.data().current_id;
                if let Some(id) = id {
                    let prev = caller.data().values.get(&key).cloned();
                    let inverses = &mut caller.data_mut().plugins[id].inverses;
                    inverses.entry(key.clone()).or_insert(prev);
                }
                caller.data_mut().values.insert(key, val);
                Ok(())
            },
        )?;
        linker.func_wrap(
            "host",
            "ctx_remove",
            |mut caller: Caller<'_, State>, kp: i32, kl: i32| -> Result<(), wasmtime::Error> {
                let key = read_str(&mut caller, kp, kl)?;
                let id = caller.data().current_id;
                if let Some(id) = id {
                    let prev = caller.data().values.get(&key).cloned();
                    let inverses = &mut caller.data_mut().plugins[id].inverses;
                    inverses.entry(key.clone()).or_insert(prev);
                }
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
        Ok(())
    }
}

/// Read a guest string with full validation: no negative ptr/len, no
/// out-of-bounds read, no non-UTF-8. Returns a trap (not a panic) on bad input.
fn read_str(
    caller: &mut Caller<'_, State>,
    ptr: i32,
    len: i32,
) -> Result<String, wasmtime::Error> {
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
