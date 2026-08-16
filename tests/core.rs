//! Integration tests mirroring the upstream Cordis core test suite.

use cordis::lua::LuaContext;

fn run(code: &str) -> Result<(), mlua::Error> {
    let app = LuaContext::new()?;
    app.run(code)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// effects
// ---------------------------------------------------------------------------

#[test]
fn effect_dispose_reverse_order() {
    // "yield dispose": disposers run in reverse order.
    run(
        r#"
        local seq = {}
        local d = ctx:effect(function()
          return {
            function() seq[#seq+1] = 1 end,
            function() seq[#seq+1] = 2 end,
            function() seq[#seq+1] = 3 end,
          }
        end)
        assert(#seq == 0)
        d()
        assert(seq[1] == 3 and seq[2] == 2 and seq[3] == 1)
        d()  -- idempotent
        assert(#seq == 3)
        "#,
    )
    .unwrap();
}

#[test]
fn effect_dispose_by_plugin() {
    run(
        r#"
        local disposed = 0
        local f = ctx:plugin(function(c)
          c:effect(function() return function() disposed = disposed + 1 end end, "test")
        end)
        assert(disposed == 0)
        f:dispose()
        assert(disposed == 1)
        f:dispose()  -- idempotent
        assert(disposed == 1)
        "#,
    )
    .unwrap();
}

#[test]
fn plugin_error_fails_fiber() {
    run(
        r#"
        local f = ctx:plugin(function(c, cfg)
          if not cfg or not cfg.ok then error("plugin error") end
        end)
        assert(f:state() == "FAILED")
        "#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// reactive coeffects
// ---------------------------------------------------------------------------

#[test]
fn inject_reacts_to_provide() {
    run(
        r#"
        local runs = 0
        ctx:inject({"foo"}, function(c)
          runs = runs + 1
        end)
        assert(runs == 0)
        local d = ctx:provide("foo", { bar = 100 })
        assert(runs == 1)
        assert(ctx.foo.bar == 100)
        d()
        assert(runs == 1)  -- unloaded, not re-run
        local d2 = ctx:provide("foo", { bar = 200 })
        assert(runs == 2)  -- re-run on re-provide
        assert(ctx.foo.bar == 200)
        d2()
        "#,
    )
    .unwrap();
}

#[test]
fn isolated_context() {
    run(
        r#"
        local runs = 0
        local plugin = function(c)
          runs = runs + 1
        end
        ctx:inject({"foo"}, plugin)
        local ctx1 = ctx:isolate("foo")
        ctx1:inject({"foo"}, plugin)
        local ctx2 = ctx:isolate("foo")
        ctx2:inject({"foo"}, plugin)

        local d0 = ctx:provide("foo", { bar = 100 })
        assert(ctx.foo.bar == 100)
        assert(runs == 1)  -- only root consumer

        local d1 = ctx1:provide("foo", { bar = 200 })
        assert(ctx1.foo.bar == 200)
        assert(runs == 2)

        d0()
        assert(runs == 2)
        assert(ctx1.foo.bar == 200)

        local d2 = ctx2:provide("foo", { bar = 300 })
        assert(ctx2.foo.bar == 300)
        assert(runs == 3)
        d1(); d2()
        "#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

#[test]
fn events_on_once_emit() {
    run(
        r#"
        local calls = 0
        local d = ctx:on("e", function() calls = calls + 1 end)
        ctx:emit("e")
        ctx:emit("e")
        assert(calls == 2)
        d()
        ctx:emit("e")
        assert(calls == 2)

        local once_calls = 0
        ctx:once("e2", function() once_calls = once_calls + 1 end)
        ctx:emit("e2")
        ctx:emit("e2")
        assert(once_calls == 1)
        "#,
    )
    .unwrap();
}

#[test]
fn events_serial_bail() {
    run(
        r#"
        local order = {}
        ctx:on("s", function(x) order[#order+1] = "a" end)
        ctx:on("s", function(x) order[#order+1] = "b"; return "stop" end)
        ctx:on("s", function(x) order[#order+1] = "c" end)
        local r = ctx:serial("s", 1)
        assert(r == "stop")
        assert(#order == 2)  -- c never ran
        assert(order[1] == "a" and order[2] == "b")
        "#,
    )
    .unwrap();
}

#[test]
fn events_waterfall() {
    run(
        r#"
        ctx:on("w", function(v, next) return v + next() end)
        ctx:on("w", function(v, next) return v + next() end)
        local r = ctx:waterfall("w", 1, function() return 2 end)
        assert(r == 4)
        "#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// inactive context
// ---------------------------------------------------------------------------

#[test]
fn inactive_context_rejects() {
    // ctx.plugin / ctx.effect on a disposed fiber's ctx must throw.
    let app = LuaContext::new().unwrap();
    app.run(
        r#"
        saved = nil
        local f = ctx:plugin(function(c)
          saved = c
          return function()
            local ok, err = pcall(function() saved:plugin(function() end) end)
            assert(not ok and err:match("inactive"))
            local ok2, err2 = pcall(function() saved:effect(function() end) end)
            assert(not ok2 and err2:match("inactive"))
          end
        end)
        f:dispose()
        "#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

#[test]
fn nested_plugins_and_registry_size() {
    run(
        r#"
        local calls = 0
        local f = ctx:plugin(function(c)
          c:on("e", function() calls = calls + 1 end)
          c:plugin(function(c2)
            c2:on("e", function() calls = calls + 1 end)
          end)
        end)
        ctx:on("e", function() calls = calls + 1 end)
        assert(calls == 0)
        ctx:emit("e")
        assert(calls == 3)
        f:dispose()
        ctx:emit("e")
        assert(calls == 4)  -- only root handler remains
        "#,
    )
    .unwrap();
}
