fn main() {
    // Emit element debug info so the Slint testing backend (and MCP server) can
    // locate elements by id.
    let config = slint_build::CompilerConfiguration::new().with_debug_info(true);
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint build failed");
}
