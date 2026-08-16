use cordis::lua::LuaContext;

fn main() -> Result<(), mlua::Error> {
    let app = LuaContext::new()?;
    let code = std::fs::read_to_string("examples/demo.lua").unwrap();
    app.run(&code)?;
    Ok(())
}
