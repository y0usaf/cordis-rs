//! Core kernel: Context, Fiber, effects, coeffects, services, events, logger.
//!
//! # The paradigm
//!
//! Cordis is *spatiotemporal composability* — two orthogonal guarantees a
//! runtime-mounted component must honor.
//!
//! **Temporal** — on unmount, every change a component made to the shared
//! environment is reversed, so nothing it owned survives it. Mechanism:
//! *revertible effects*. An effect returns its own inverse (a disposer); the
//! runtime tracks inverses and applies them in reverse order — last mounted,
//! first torn down — so a component's cleanup runs before the cleanup of
//! whatever it depended on.
//!
//! **Spatial** — a component declares the context keys it reads (its coeffect
//! spec), and the runtime notifies exactly those components when a key changes.
//! Mechanism: *reactive coeffects*. `provide`/`unprovide` mutate the service
//! store; `notify` re-resolves every component whose spec names the key.
//!
//! Both share one host-owned state — the `Context` — which effects mutate and
//! coeffects name. That single context type *is* the paradigm.
//!
//! # Ownership
//!
//! All `Rc`, single-threaded (Lua is not `Send`). The one cycle — `Fiber` owns
//! its `Context`, `Context` owns its `Fiber` — is broken on the context→fiber
//! edge: `Context.fiber` is `Weak`, so a component is freed the moment its
//! parent drops it. No leak per mount/unmount.
//!
//! # Why an epoch string
//!
//! A fiber's `epoch` fingerprints its resolved dependencies (a string of their
//! uids). `refresh` answers one question — "did the dependency set change?" —
//! by string comparison. A dep appearing, vanishing, or re-providing under a
//! new uid changes the epoch and reloads the fiber. A bool would miss the
//! re-provide case (same count, new identity).

use mlua::{Function, Lua, MultiValue, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// ids
// ---------------------------------------------------------------------------

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// disposables + effects
// ---------------------------------------------------------------------------

pub struct EffectInner {
    pub label: String,
    pub disposables: RefCell<Vec<Disposable>>,
    pub ran: Cell<bool>,
}

impl EffectInner {
    pub fn new(label: String) -> Rc<Self> {
        Rc::new(EffectInner {
            label,
            disposables: RefCell::new(Vec::new()),
            ran: Cell::new(false),
        })
    }
    /// Run collected disposers once, in reverse. Reversal is the temporal
    /// guarantee: the last effect mounted is the first undone, so teardown
    /// never runs against a half-removed environment.
    pub fn run(&self) {
        if self.ran.replace(true) {
            return;
        }
        let mut v = std::mem::take(&mut *self.disposables.borrow_mut());
        v.reverse();
        for d in v {
            d.run();
        }
    }
}

pub enum DisposableKind {
    Raw(Option<Box<dyn FnOnce()>>),
    Effect(Rc<EffectInner>),
}

pub struct Disposable {
    pub id: u64,
    pub kind: DisposableKind,
}

impl Disposable {
    pub fn raw(f: impl FnOnce() + 'static) -> Self {
        Disposable { id: next_id(), kind: DisposableKind::Raw(Some(Box::new(f))) }
    }
    pub fn effect(inner: Rc<EffectInner>) -> Self {
        Disposable { id: next_id(), kind: DisposableKind::Effect(inner) }
    }
    pub fn effect_inner(&self) -> Option<&Rc<EffectInner>> {
        match &self.kind {
            DisposableKind::Effect(e) => Some(e),
            _ => None,
        }
    }
    pub fn run(self) {
        match self.kind {
            DisposableKind::Raw(f) => {
                if let Some(f) = f {
                    f();
                }
            }
            DisposableKind::Effect(e) => e.run(),
        }
    }
}

/// Ordered list of disposers; `clear()` drains in reverse (LIFO unmount).
pub struct DisposableList {
    items: Vec<Disposable>,
}

impl DisposableList {
    pub fn new() -> Self {
        DisposableList { items: Vec::new() }
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn push(&mut self, d: Disposable) {
        self.items.push(d);
    }
    pub fn delete(&mut self, id: u64) -> bool {
        if let Some(i) = self.items.iter().position(|d| d.id == id) {
            self.items.remove(i);
            true
        } else {
            false
        }
    }
    pub fn clear(&mut self) -> Vec<Disposable> {
        let mut v = std::mem::take(&mut self.items);
        v.reverse();
        v
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Disposable> {
        self.items.iter()
    }
}

// ---------------------------------------------------------------------------
// fiber
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FiberState {
    Pending,
    Active,
    Failed,
    Disposed,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Epoch {
    Inactive,
    Active(String),
}

pub struct Fiber {
    pub uid: Cell<Option<u64>>,
    pub parent: Rc<Context>,
    pub ctx: Rc<Context>,
    pub config: RefCell<Value>,
    pub inject: HashMap<String, Option<Value>>,
    pub runtime: Option<Rc<Runtime>>,
    pub state: Cell<FiberState>,
    pub disposables: Rc<RefCell<DisposableList>>,
    pub epoch: RefCell<Epoch>,
    pub error: RefCell<Option<String>>,
    pub callback: Option<Function>,
    /// The effect wrapper registered with the parent fiber (plugin cleanup).
    pub dispose_effect: RefCell<Option<Rc<EffectInner>>>,
}

impl Fiber {
    pub fn new_root(parent: Rc<Context>) -> Rc<Self> {
        let fiber = Rc::new(Fiber {
            uid: Cell::new(Some(0)),
            parent: parent.clone(),
            ctx: parent.clone(),
            config: RefCell::new(Value::Nil),
            inject: HashMap::new(),
            runtime: None,
            state: Cell::new(FiberState::Active),
            disposables: Rc::new(RefCell::new(DisposableList::new())),
            epoch: RefCell::new(Epoch::Active(String::new())),
            error: RefCell::new(None),
            callback: None,
            dispose_effect: RefCell::new(None),
        });
        *parent.fiber.borrow_mut() = Rc::downgrade(&fiber);
        fiber
    }

    pub fn name(&self) -> String {
        "root".to_string()
    }

    pub fn assert_active(&self) -> Result<(), mlua::Error> {
        if self.uid.get().is_none() {
            return Err(mlua::Error::RuntimeError(
                "cannot create effect on inactive context".to_string(),
            ));
        }
        Ok(())
    }

    fn get_state(&self) -> FiberState {
        if self.uid.get().is_none() {
            return FiberState::Disposed;
        }
        if self.error.borrow().is_some() {
            return FiberState::Failed;
        }
        if *self.epoch.borrow() != Epoch::Inactive {
            return FiberState::Active;
        }
        FiberState::Pending
    }

    /// Run the plugin callback, collecting its effect into `self.disposables`.
    fn reload(&self) {
        match self.run_callback() {
            Ok(()) => {
                self.error.replace(None);
            }
            Err(e) => {
                self.error.replace(Some(format!("{e}")));
            }
        }
        self.state.set(self.get_state());
    }

    fn run_callback(&self) -> Result<(), mlua::Error> {
        let Some(cb) = self.callback.clone() else {
            return Ok(());
        };
        let ctx_ud = crate::lua::Ctx(self.ctx.clone());
        let config = self.config.borrow().clone();
        let result: MultiValue = cb.call((ctx_ud, config))?;
        let mut collected: Vec<Disposable> = Vec::new();
        collect_effect_return(result, &mut collected);
        for d in collected {
            self.disposables.borrow_mut().push(d);
        }
        Ok(())
    }

    fn unload(&self) {
        let disposers = self.disposables.borrow_mut().clear();
        for d in disposers {
            d.run();
        }
        self.state.set(self.get_state());
    }

    fn set_epoch(&self, epoch: Epoch) {
        let old = self.epoch.replace(epoch.clone());
        if epoch == old {
            return;
        }
        if epoch != Epoch::Inactive {
            self.reload();
        } else {
            self.unload();
        }
    }

    pub fn refresh(&self) {
        let mut epoch = Epoch::Active(String::new());
        for name in self.inject.keys() {
            match self.ctx.shared.reflect.get_impl(self.ctx.isolate_key(name)) {
                Some(impl_) => {
                    if let Epoch::Active(s) = &mut epoch {
                        s.push(':');
                        s.push_str(&impl_.fiber.uid.get().unwrap_or(0).to_string());
                    }
                }
                None => {
                    epoch = Epoch::Inactive;
                    break;
                }
            }
        }
        self.set_epoch(epoch);
    }

    /// Cleanup on dispose: detach from runtime, unload own disposers.
    fn cleanup(&self, remove: Box<dyn FnOnce()>) {
        self.uid.set(None);
        self.ctx.active.set(false);
        remove();
        if let Some(rt) = &self.runtime {
            if rt.fibers.borrow().is_empty() {
                self.ctx.shared.registry.delete_runtime(rt);
            }
        }
        self.set_epoch(Epoch::Inactive);
    }

    pub fn dispose(&self) {
        if let Some(e) = self.dispose_effect.borrow().as_ref() {
            e.run();
        }
    }

    pub fn restart(&self) {
        self.assert_active().unwrap();
        self.set_epoch(Epoch::Inactive);
        self.refresh();
    }

    pub fn update(&self, config: Value) {
        self.assert_active().unwrap();
        *self.config.borrow_mut() = config;
        self.error.replace(None);
        self.restart();
    }

    pub fn get_effects(&self) -> Vec<String> {
        self.disposables
            .borrow()
            .iter()
            .filter_map(|d| d.effect_inner().map(|e| e.label.clone()))
            .collect()
    }

    /// Rust-side effect: run `execute`, collect its disposers.
    pub fn effect_rust<F>(&self, execute: F, label: &str) -> Rc<EffectInner>
    where
        F: FnOnce(&mut Collector),
    {
        self.assert_active().unwrap();
        let inner = EffectInner::new(label.to_string());
        let mut collector = Collector {
            inner: inner.clone(),
            fiber_list: self.disposables.clone(),
        };
        execute(&mut collector);
        finish_effect(inner.clone(), self.disposables.clone());
        inner
    }
}

/// Collects disposers produced by an effect's execute phase.
pub struct Collector {
    inner: Rc<EffectInner>,
    fiber_list: Rc<RefCell<DisposableList>>,
}

impl Collector {
    pub fn collect(&mut self, d: Disposable) {
        self.fiber_list.borrow_mut().delete(d.id);
        self.inner.disposables.borrow_mut().push(d);
    }
    pub fn collect_raw(&mut self, f: impl FnOnce() + 'static) {
        self.inner.disposables.borrow_mut().push(Disposable::raw(f));
    }
}

/// Register the effect wrapper with the fiber and add its unregister as a
/// disposer of the effect itself (so disposing the effect unregisters it).
fn finish_effect(inner: Rc<EffectInner>, fiber_list: Rc<RefCell<DisposableList>>) {
    let wid = next_id();
    fiber_list.borrow_mut().push(Disposable {
        id: wid,
        kind: DisposableKind::Effect(inner.clone()),
    });
    inner.disposables.borrow_mut().push(Disposable::raw(move || {
        fiber_list.borrow_mut().delete(wid);
    }));
}

/// Interpret a plugin callback's Lua return as a list of disposers.
fn collect_effect_return(result: MultiValue, list: &mut Vec<Disposable>) {
    let mut vals = result.into_iter();
    let Some(v) = vals.next() else { return };
    match v {
        Value::Nil => {}
        Value::Function(f) => {
            list.push(Disposable::raw(move || {
                let _ = f.call::<()>(());
            }));
        }
        Value::Table(t) => {
            let mut i = 1i64;
            loop {
                let v: Value = match t.get(i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                if v.is_nil() {
                    break;
                }
                if let Value::Function(f) = v {
                    list.push(Disposable::raw(move || {
                        let _ = f.call::<()>(());
                    }));
                }
                i += 1;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// runtime + registry
// ---------------------------------------------------------------------------

pub struct Runtime {
    pub callback: Function,
    pub fibers: RefCell<Vec<Rc<Fiber>>>,
}

impl Runtime {
    fn push_fiber(self: &Rc<Self>, f: Rc<Fiber>) -> Box<dyn FnOnce()> {
        self.fibers.borrow_mut().push(f.clone());
        let me = self.clone();
        Box::new(move || {
            let mut fibers = me.fibers.borrow_mut();
            fibers.retain(|x| !Rc::ptr_eq(x, &f));
        })
    }
}

pub struct RegistryService {
    pub counter: Cell<u64>,
    pub runtimes: RefCell<Vec<(Function, Rc<Runtime>)>>,
}

impl RegistryService {
    pub fn new() -> Self {
        RegistryService { counter: Cell::new(0), runtimes: RefCell::new(Vec::new()) }
    }
    pub fn size(&self) -> usize {
        self.runtimes.borrow().len()
    }
    fn find(&self, cb: &Function) -> Option<Rc<Runtime>> {
        self.runtimes
            .borrow()
            .iter()
            .find(|(f, _)| f == cb)
            .map(|(_, rt)| rt.clone())
    }
    pub fn delete_runtime(&self, rt: &Rc<Runtime>) {
        let mut runtimes = self.runtimes.borrow_mut();
        runtimes.retain(|(_, r)| !Rc::ptr_eq(r, rt));
    }
    pub fn delete(&self, cb: &Function) {
        if let Some(rt) = self.find(cb) {
            let fibers = std::mem::take(&mut *rt.fibers.borrow_mut());
            for f in fibers {
                f.dispose();
            }
            self.delete_runtime(&rt);
        }
    }
}

// ---------------------------------------------------------------------------
// reflect (service store)
// ---------------------------------------------------------------------------

pub struct Impl {
    pub name: String,
    pub fiber: Rc<Fiber>,
    pub value: RefCell<Value>,
}

pub struct ReflectService {
    /// keyed by isolate symbol (u64)
    pub store: RefCell<HashMap<u64, Rc<Impl>>>,
}

impl ReflectService {
    pub fn new() -> Self {
        ReflectService { store: RefCell::new(HashMap::new()) }
    }
    pub fn get_impl(&self, key: u64) -> Option<Rc<Impl>> {
        self.store.borrow().get(&key).cloned()
    }
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

pub struct Hook {
    pub callback: Function,
}

pub struct EventsService {
    pub hooks: RefCell<HashMap<String, Vec<Hook>>>,
}

impl EventsService {
    pub fn new() -> Self {
        EventsService { hooks: RefCell::new(HashMap::new()) }
    }
}

// ---------------------------------------------------------------------------
// logger
// ---------------------------------------------------------------------------

pub struct Message {
    pub sn: u64,
    pub name: String,
    pub level: u32,
    pub args: Vec<Value>,
}

pub struct LoggerService {
    pub buffer: RefCell<Vec<Message>>,
    pub sn: Cell<u64>,
}

impl LoggerService {
    pub fn new() -> Self {
        LoggerService { buffer: RefCell::new(Vec::new()), sn: Cell::new(0) }
    }
}

// ---------------------------------------------------------------------------
// context
// ---------------------------------------------------------------------------

pub struct Shared {
    pub lua: Lua,
    /// canonical isolate symbols (root-owned)
    pub isolate_root: RefCell<HashMap<String, u64>>,
    pub root_fiber: RefCell<Option<Rc<Fiber>>>,
    pub registry: RegistryService,
    pub reflect: ReflectService,
    pub events: EventsService,
    pub logger: LoggerService,
    pub symbol_counter: Cell<u64>,
}

pub struct Context {
    pub shared: Rc<Shared>,
    pub parent: Option<Rc<Context>>,
    pub fiber: RefCell<Weak<Fiber>>,
    pub active: Cell<bool>,
    /// isolate symbol overrides (shadow the chain)
    pub isolate: RefCell<HashMap<String, u64>>,
}

impl Context {
    pub fn new_root(lua: &Lua) -> Rc<Self> {
        let shared = Rc::new(Shared {
            lua: lua.clone(),
            isolate_root: RefCell::new(HashMap::new()),
            root_fiber: RefCell::new(None),
            registry: RegistryService::new(),
            reflect: ReflectService::new(),
            events: EventsService::new(),
            logger: LoggerService::new(),
            symbol_counter: Cell::new(1),
        });
        let ctx = Rc::new(Context {
            shared,
            parent: None,
            fiber: RefCell::new(Weak::new()),
            active: Cell::new(true),
            isolate: RefCell::new(HashMap::new()),
        });
        let fiber = Fiber::new_root(ctx.clone());
        *ctx.shared.root_fiber.borrow_mut() = Some(fiber);
        ctx
    }

    pub fn fiber(&self) -> Rc<Fiber> {
        if let Some(f) = self.fiber.borrow().upgrade() {
            return f;
        }
        self.shared.root_fiber.borrow().as_ref().unwrap().clone()
    }

    /// Resolve the isolate symbol for `name` by walking the chain. The symbol
    /// is the spatial identity of a service: two contexts that resolve the same
    /// name to different symbols see different services — how `isolate` scopes a
    /// dependency to a subtree without touching the root.
    pub fn isolate_key(&self, name: &str) -> u64 {
        let mut ctx = Some(self);
        while let Some(c) = ctx {
            if let Some(&k) = c.isolate.borrow().get(name) {
                return k;
            }
            ctx = c.parent.as_deref();
        }
        self.shared.isolate_root.borrow().get(name).copied().unwrap_or(0)
    }

    fn new_symbol(&self) -> u64 {
        let s = self.shared.symbol_counter.get();
        self.shared.symbol_counter.set(s + 1);
        s
    }

    /// Child context (prototype chain) with empty overrides.
    pub fn extend(self: Rc<Context>) -> Rc<Context> {
        Rc::new(Context {
            shared: self.shared.clone(),
            parent: Some(self.clone()),
            fiber: RefCell::new(Weak::new()),
            active: Cell::new(true),
            isolate: RefCell::new(HashMap::new()),
        })
    }

    pub fn isolate(self: Rc<Context>, name: &str, label: Option<u64>) -> Rc<Context> {
        let sym = label.unwrap_or_else(|| self.new_symbol());
        let child = self.extend();
        child.isolate.borrow_mut().insert(name.to_string(), sym);
        child
    }

    pub fn assert_active(&self) -> Result<(), mlua::Error> {
        if !self.active.get() {
            return Err(mlua::Error::RuntimeError(
                "cannot create effect on inactive context".to_string(),
            ));
        }
        Ok(())
    }

    // -- effects ------------------------------------------------------------

    /// Lua-side effect: run `execute` (a Lua function), collect its return as
    /// disposers, and register a wrapper with the fiber.
    pub fn effect(&self, execute: Function, label: &str) -> Result<Rc<EffectInner>, mlua::Error> {
        self.assert_active()?;
        let fiber = self.fiber();
        let inner = EffectInner::new(label.to_string());
        let result: MultiValue = execute.call::<MultiValue>(())?;
        let mut collected: Vec<Disposable> = Vec::new();
        collect_effect_return(result, &mut collected);
        for d in collected {
            fiber.disposables.borrow_mut().delete(d.id);
            inner.disposables.borrow_mut().push(d);
        }
        finish_effect(inner.clone(), fiber.disposables.clone());
        Ok(inner)
    }

    // -- services -----------------------------------------------------------

    pub fn provide(&self, name: &str, value: Value) -> Result<Rc<EffectInner>, mlua::Error> {
        self.assert_active()?;
        let fiber = self.fiber();
        let name = name.to_string();
        let label = format!("ctx.provide({name:?})");
        let shared = self.shared.clone();
        {
            let mut root = shared.isolate_root.borrow_mut();
            root.entry(name.clone()).or_insert_with(|| {
                let s = shared.symbol_counter.get();
                shared.symbol_counter.set(s + 1);
                s
            });
        }
        let key = self.isolate_key(&name);
        let lua = shared.lua.clone();
        let execute = lua.create_function(move |_, _: ()| {
            let impl_ = Rc::new(Impl {
                name: name.clone(),
                fiber: fiber.clone(),
                value: RefCell::new(value.clone()),
            });
            shared.reflect.store.borrow_mut().insert(key, impl_.clone());
            if fiber.state.get() == FiberState::Active {
                notify(shared.clone(), &[name.clone()]);
            }
            let shared2 = shared.clone();
            let name2 = name.clone();
            let key2 = key;
            let lua2 = shared2.lua.clone();
            Ok(Value::Function(lua2.create_function(move |_, _: ()| {
                shared2.reflect.store.borrow_mut().remove(&key2);
                notify(shared2.clone(), &[name2.clone()]);
                Ok(())
            })?))
        })?;
        self.effect(execute, &label)
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let key = self.isolate_key(name);
        let impl_ = self.shared.reflect.get_impl(key)?;
        let value = impl_.value.borrow();
        Some((*value).clone())
    }

    pub fn set(&self, name: &str, value: Value) -> Result<(), String> {
        let key = self.isolate_key(name);
        let impl_ = self.shared.reflect.get_impl(key).ok_or_else(|| {
            format!("cannot set property \"{name}\" without provide")
        })?;
        if !Rc::ptr_eq(&impl_.fiber, &self.fiber()) {
            return Err(format!("cannot set property \"{name}\" in multiple fibers"));
        }
        *impl_.value.borrow_mut() = value;
        Ok(())
    }

    /// Coeffect resolution: `ctx.foo` resolves the impl under this context's
    /// isolate key, then walks the fiber chain only to produce error messages.
    pub fn resolve_property(&self, name: &str) -> Result<Value, String> {
        let key = self.isolate_key(name);
        if let Some(impl_) = self.shared.reflect.get_impl(key) {
            if impl_.fiber.state.get() == FiberState::Active {
                let value = impl_.value.borrow();
                return Ok((*value).clone());
            }
        }
        let mut fiber = self.fiber();
        loop {
            if fiber.inject.contains_key(name) {
                return Err(format!(
                    "cannot get required service \"{name}\" in inactive context"
                ));
            }
            if fiber.runtime.is_none() {
                return Err(format!("cannot get property \"{name}\" without inject"));
            }
            if fiber.parent.isolate_key(name) != key {
                return Err(format!("cannot get property \"{name}\" without inject"));
            }
            fiber = fiber.parent.fiber();
        }
    }

    // -- plugins ------------------------------------------------------------

    pub fn plugin(
        self: Rc<Context>,
        callback: Function,
        config: Value,
        inject: HashMap<String, Option<Value>>,
    ) -> Result<Rc<Fiber>, mlua::Error> {
        self.assert_active()?;
        let shared = self.shared.clone();
        let parent_fiber = self.fiber();
        let parent_ctx = parent_fiber.ctx.clone();
        let runtime = {
            let mut runtimes = shared.registry.runtimes.borrow_mut();
            match runtimes.iter().find(|(f, _)| *f == callback) {
                Some((_, rt)) => rt.clone(),
                None => {
                    let rt = Rc::new(Runtime {
                        callback: callback.clone(),
                        fibers: RefCell::new(Vec::new()),
                    });
                    runtimes.push((callback.clone(), rt.clone()));
                    rt
                }
            }
        };
        let uid = shared.registry.counter.get() + 1;
        shared.registry.counter.set(uid);

        let ctx = self.extend();
        ctx.active.set(true);

        let fiber = Rc::new(Fiber {
            uid: Cell::new(Some(uid)),
            parent: parent_ctx,
            ctx: ctx.clone(),
            config: RefCell::new(config),
            inject: inject.clone(),
            runtime: Some(runtime.clone()),
            state: Cell::new(FiberState::Pending),
            disposables: Rc::new(RefCell::new(DisposableList::new())),
            epoch: RefCell::new(Epoch::Inactive),
            error: RefCell::new(None),
            callback: Some(callback),
            dispose_effect: RefCell::new(None),
        });
        *ctx.fiber.borrow_mut() = Rc::downgrade(&fiber);

        let parent_fiber = parent_fiber;
        let fiber2 = fiber.clone();
        let runtime2 = runtime.clone();
        let effect = parent_fiber.effect_rust(
            move |c| {
                let remove = runtime2.push_fiber(fiber2.clone());
                fiber2.refresh();
                let f = fiber2.clone();
                c.collect_raw(move || f.cleanup(remove));
            },
            "ctx.plugin()",
        );
        *fiber.dispose_effect.borrow_mut() = Some(effect);

        Ok(fiber)
    }

    // -- events -------------------------------------------------------------

    pub fn on(
        &self,
        name: &str,
        listener: Function,
    ) -> Result<Rc<EffectInner>, mlua::Error> {
        self.assert_active()?;
        let name = name.to_string();
        let shared = self.shared.clone();
        let label = format!("ctx.on({name:?})");
        let lua = shared.lua.clone();
        let execute = lua.create_function(move |_, _: ()| {
            let hook = Hook { callback: listener.clone() };
            {
                let mut hooks = shared.events.hooks.borrow_mut();
                hooks.entry(name.clone()).or_default().push(hook);
            }
            let shared2 = shared.clone();
            let name2 = name.clone();
            let listener2 = listener.clone();
            let lua2 = shared2.lua.clone();
            Ok(Value::Function(lua2.create_function(move |_, _: ()| {
                let mut hooks = shared2.events.hooks.borrow_mut();
                if let Some(list) = hooks.get_mut(&name2) {
                    list.retain(|h| h.callback != listener2);
                }
                Ok(())
            })?))
        })?;
        self.effect(execute, &label)
    }

    pub fn once(&self, name: &str, listener: Function) -> Result<Rc<EffectInner>, mlua::Error> {
        let dispose_cell: Rc<RefCell<Option<Rc<EffectInner>>>> = Rc::new(RefCell::new(None));
        let shared = self.shared.clone();
        let listener2 = listener.clone();
        let cell2 = dispose_cell.clone();
        let wrapped = shared.lua.create_function(move |_, args: MultiValue| {
            if let Some(e) = cell2.borrow().as_ref() {
                e.run();
            }
            listener2.call::<MultiValue>(args)
        })?;
        let effect = self.on(name, wrapped)?;
        *dispose_cell.borrow_mut() = Some(effect.clone());
        Ok(effect)
    }

    fn resolve_hooks(&self, name: &str) -> Vec<Function> {
        let hooks = self.shared.events.hooks.borrow();
        hooks
            .get(name)
            .map(|hs| hs.iter().map(|h| h.callback.clone()).collect())
            .unwrap_or_default()
    }

    pub fn emit(&self, name: &str, args: MultiValue) -> Result<(), mlua::Error> {
        let callbacks = self.resolve_hooks(name);
        for cb in callbacks {
            let _ = cb.call::<MultiValue>(args.clone())?;
        }
        Ok(())
    }

    pub fn serial(&self, name: &str, args: MultiValue) -> Result<Value, mlua::Error> {
        let callbacks = self.resolve_hooks(name);
        for cb in callbacks {
            let r: MultiValue = cb.call(args.clone())?;
            if let Some(v) = r.into_iter().next() {
                if !(v.is_nil() || v == Value::Boolean(false)) {
                    return Ok(v);
                }
            }
        }
        Ok(Value::Nil)
    }

    pub fn bail(&self, name: &str, args: MultiValue) -> Result<Value, mlua::Error> {
        self.serial(name, args)
    }

    pub fn parallel(&self, name: &str, args: MultiValue) -> Result<(), mlua::Error> {
        let callbacks = self.resolve_hooks(name);
        let mut errors: Vec<String> = Vec::new();
        for cb in callbacks {
            if let Err(e) = cb.call::<MultiValue>(args.clone()) {
                errors.push(format!("{e}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(mlua::Error::RuntimeError(format!(
                "AggregateError: {}",
                errors.join("; ")
            )))
        }
    }

    pub fn waterfall(
        &self,
        name: &str,
        args: MultiValue,
        final_fn: Function,
    ) -> Result<Value, mlua::Error> {
        let callbacks = Rc::new(self.resolve_hooks(name));
        let shared = self.shared.clone();
        let idx = Rc::new(Cell::new(0usize));
        let args = Rc::new(args);
        let final_fn = Rc::new(final_fn);
        let next_cell: Rc<RefCell<Option<Function>>> = Rc::new(RefCell::new(None));

        // `next` advances one callback per call; it holds itself via a cell to
        // break the self-reference (a closure cannot capture its own handle).
        let next = {
            let cell = next_cell.clone();
            let (callbacks, idx, args, final_fn) =
                (callbacks.clone(), idx.clone(), args.clone(), final_fn.clone());
            shared.lua.create_function(move |_, _: ()| {
                let i = idx.get();
                if i < callbacks.len() {
                    idx.set(i + 1);
                    let cb = callbacks[i].clone();
                    let mut a = args.as_ref().clone();
                    let guard = cell.borrow();
                    a.push_back(Value::Function(guard.as_ref().unwrap().clone()));
                    cb.call::<Value>(a)
                } else {
                    final_fn.call::<Value>(args.as_ref().clone())
                }
            })?
        };
        *next_cell.borrow_mut() = Some(next.clone());

        let mut a = args.as_ref().clone();
        a.push_back(Value::Function(next));
        let i = idx.get();
        if i < callbacks.len() {
            idx.set(i + 1);
            callbacks[i].call::<Value>(a)
        } else {
            final_fn.call::<Value>(args.as_ref().clone())
        }
    }

    // -- logging ------------------------------------------------------------

    pub fn log(&self, level: u32, name: &str, args: Vec<Value>) {
        let sn = self.shared.logger.sn.get() + 1;
        self.shared.logger.sn.set(sn);
        self.shared.logger.buffer.borrow_mut().push(Message {
            sn,
            name: name.to_string(),
            level,
            args,
        });
    }
}

// ---------------------------------------------------------------------------
// notify (reactive coeffects)
// ---------------------------------------------------------------------------

/// Reactive coeffect engine. After a context change names a set of keys, walk
/// every mounted fiber and re-resolve any whose inject spec names one of those
/// keys. A fiber that lost a dependency unloads; one that gained one reloads.
pub fn notify(shared: Rc<Shared>, names: &[String]) {
    let runtimes: Vec<Rc<Runtime>> = shared
        .registry
        .runtimes
        .borrow()
        .iter()
        .map(|(_, rt)| rt.clone())
        .collect();
    for rt in runtimes {
        let fibers: Vec<Rc<Fiber>> = rt.fibers.borrow().clone();
        for fiber in fibers {
            if names.iter().any(|name| fiber.inject.contains_key(name)) {
                fiber.refresh();
            }
        }
    }
}
