//! Lua bindings: expose Context, Fiber, and effects as UserData.

use crate::core::{Context, EffectInner, Fiber, FiberState};
use mlua::{Function, Lua, MetaMethod, MultiValue, UserData, UserDataMethods, Value};
use std::collections::HashMap;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Context userdata
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Ctx(pub Rc<Context>);

impl UserData for Ctx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method(
            "plugin",
            |_lua, this, (callback, config): (Function, Option<Value>)| {
                let ctx = this.0.clone();
                async move {
                    let config = config.unwrap_or(Value::Nil);
                    let fiber = ctx.plugin(callback, config, HashMap::new()).await?;
                    Ok(FiberHandle(fiber))
                }
            },
        );
        methods.add_async_method(
            "inject",
            |_lua, this, (deps, callback): (Value, Function)| {
                let ctx = this.0.clone();
                async move {
                    let inject = parse_inject(&deps);
                    let fiber = ctx.plugin(callback, Value::Nil, inject).await?;
                    Ok(FiberHandle(fiber))
                }
            },
        );
        methods.add_async_method(
            "effect",
            |_lua, this, (execute, label): (Function, Option<String>)| {
                let ctx = this.0.clone();
                async move {
                    let label = label.unwrap_or_else(|| "anonymous".to_string());
                    let inner = ctx.effect(execute, &label).await?;
                    Ok(EffectHandle(inner))
                }
            },
        );
        methods.add_async_method(
            "on",
            |_lua, this, (name, listener): (String, Function)| {
                let ctx = this.0.clone();
                async move {
                    let inner = ctx.on(&name, listener).await?;
                    Ok(EffectHandle(inner))
                }
            },
        );
        methods.add_async_method(
            "once",
            |_lua, this, (name, listener): (String, Function)| {
                let ctx = this.0.clone();
                async move {
                    let inner = ctx.once(&name, listener).await?;
                    Ok(EffectHandle(inner))
                }
            },
        );
        methods.add_async_method(
            "provide",
            |_lua, this, (name, value): (String, Option<Value>)| {
                let ctx = this.0.clone();
                async move {
                    let value = value.unwrap_or(Value::Nil);
                    let inner = ctx.provide(&name, value).await?;
                    Ok(EffectHandle(inner))
                }
            },
        );
        methods.add_method("get", |_lua, this, name: String| {
            Ok(this.0.get(&name).unwrap_or(Value::Nil))
        });
        methods.add_method("set", |_lua, this, (name, value): (String, Value)| {
            this.0
                .set(&name, value)
                .map_err(mlua::Error::RuntimeError)?;
            Ok(())
        });
        methods.add_method("isolate", |_lua, this, name: String| {
            Ok(Ctx(this.0.clone().isolate(&name, None)))
        });
        methods.add_method("extend", |_lua, this, _: ()| {
            Ok(Ctx(this.0.clone().extend()))
        });
        methods.add_async_method("emit", |_lua, this, mut args: MultiValue| {
            let ctx = this.0.clone();
            async move {
                let name = pop_name(&mut args)?;
                ctx.emit(&name, args).await?;
                Ok(())
            }
        });
        methods.add_async_method("parallel", |_lua, this, mut args: MultiValue| {
            let ctx = this.0.clone();
            async move {
                let name = pop_name(&mut args)?;
                ctx.parallel(&name, args).await?;
                Ok(())
            }
        });
        methods.add_async_method("serial", |_lua, this, mut args: MultiValue| {
            let ctx = this.0.clone();
            async move {
                let name = pop_name(&mut args)?;
                ctx.serial(&name, args).await
            }
        });
        methods.add_async_method("bail", |_lua, this, mut args: MultiValue| {
            let ctx = this.0.clone();
            async move {
                let name = pop_name(&mut args)?;
                ctx.bail(&name, args).await
            }
        });
        methods.add_async_method("waterfall", |_lua, this, mut args: MultiValue| {
            let ctx = this.0.clone();
            async move {
                let name = pop_name(&mut args)?;
                let final_fn = pop_fn(&mut args)?;
                ctx.waterfall(&name, args, final_fn).await
            }
        });
        methods.add_method("logger", |_lua, this, _: ()| Ok(Logger(this.0.clone())));

        // coeffect resolution: ctx.foo
        methods.add_meta_method(MetaMethod::Index, |_lua, this, key: String| {
            this.0
                .resolve_property(&key)
                .map_err(mlua::Error::RuntimeError)
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_lua, this, (key, value): (String, Value)| {
                this.0.set(&key, value).map_err(mlua::Error::RuntimeError)?;
                Ok(())
            },
        );
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, _: ()| {
            Ok(format!("Context <{}>", this.0.fiber().name()))
        });
    }
}

fn pop_name(args: &mut MultiValue) -> Result<String, mlua::Error> {
    let v = args
        .pop_front()
        .ok_or_else(|| mlua::Error::RuntimeError("missing event name".into()))?;
    v.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| mlua::Error::RuntimeError("event name must be a string".into()))
}

fn pop_fn(args: &mut MultiValue) -> Result<Function, mlua::Error> {
    let v = args
        .pop_back()
        .ok_or_else(|| mlua::Error::RuntimeError("missing final function".into()))?;
    match v {
        Value::Function(f) => Ok(f),
        _ => Err(mlua::Error::RuntimeError(
            "final argument must be a function".into(),
        )),
    }
}

/// Parse a Lua inject spec: an array of names, or a map name -> config.
fn parse_inject(v: &Value) -> HashMap<String, Option<Value>> {
    let mut result = HashMap::new();
    if let Value::Table(t) = v {
        let mut is_array = false;
        if let Ok(first) = t.get::<Value>(1) {
            if !first.is_nil() {
                is_array = true;
            }
        }
        if is_array {
            let mut i = 1i64;
            loop {
                let name: Value = match t.get(i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                if name.is_nil() {
                    break;
                }
                if let Some(s) = name.as_str() {
                    result.insert(s.to_string(), None);
                }
                i += 1;
            }
        } else {
            for pair in t.pairs::<String, Value>() {
                if let Ok((name, config)) = pair {
                    result.insert(name, Some(config));
                }
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Fiber userdata
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FiberHandle(pub Rc<Fiber>);

impl UserData for FiberHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("dispose", |_lua, this, _: ()| {
            let fiber = this.0.clone();
            async move {
                fiber.dispose().await;
                Ok(())
            }
        });
        methods.add_async_method("restart", |_lua, this, _: ()| {
            let fiber = this.0.clone();
            async move {
                fiber.restart().await;
                Ok(())
            }
        });
        methods.add_async_method("update", |_lua, this, config: Value| {
            let fiber = this.0.clone();
            async move {
                fiber.update(config).await;
                Ok(())
            }
        });
        methods.add_method("name", |_lua, this, _: ()| Ok(this.0.name()));
        methods.add_method("state", |_lua, this, _: ()| {
            Ok(state_name(this.0.state.get()))
        });
        methods.add_method("uid", |_lua, this, _: ()| {
            Ok(this.0.uid.get().map(|u| u as i64).unwrap_or(-1))
        });
        methods.add_method("get_effects", |lua, this, _: ()| {
            let table = lua.create_table()?;
            for (i, label) in this.0.get_effects().into_iter().enumerate() {
                table.set(i + 1, label)?;
            }
            Ok(table)
        });
    }
}

fn state_name(s: FiberState) -> &'static str {
    match s {
        FiberState::Pending => "PENDING",
        FiberState::Loading => "LOADING",
        FiberState::Active => "ACTIVE",
        FiberState::Failed => "FAILED",
        FiberState::Disposed => "DISPOSED",
        FiberState::Unloading => "UNLOADING",
    }
}

// ---------------------------------------------------------------------------
// Effect userdata (disposer)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct EffectHandle(pub Rc<EffectInner>);

impl UserData for EffectHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_meta_method(MetaMethod::Call, |_lua, this, _: ()| {
            let inner = this.0.clone();
            async move {
                inner.run().await;
                Ok(Value::Nil)
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Logger userdata
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Logger(pub Rc<Context>);

impl UserData for Logger {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        for (level, name) in [
            (0u32, "error"),
            (1u32, "warn"),
            (2u32, "info"),
            (3u32, "debug"),
        ] {
            let name = name.to_string();
            methods.add_method(&name, move |_lua, this, args: MultiValue| {
                let vals: Vec<Value> = args.into_iter().collect();
                this.0.log(level, "root", vals);
                Ok(())
            });
        }
    }
}

// ---------------------------------------------------------------------------
// top-level entry
// ---------------------------------------------------------------------------

pub struct LuaContext {
    pub lua: Lua,
    pub ctx: Rc<Context>,
}

impl LuaContext {
    pub fn new() -> Result<Self, mlua::Error> {
        let lua = Lua::new();
        let ctx = Context::new_root(&lua);
        lua.globals().set("ctx", Ctx(ctx.clone()))?;
        Ok(LuaContext { lua, ctx })
    }

    pub async fn run_async(&self, code: &str) -> Result<MultiValue, mlua::Error> {
        self.lua.load(code).eval_async().await
    }

    pub fn run(&self, code: &str) -> Result<MultiValue, mlua::Error> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        rt.block_on(self.run_async(code))
    }
}
