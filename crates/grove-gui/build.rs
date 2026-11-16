fn main() {
    // `fluent` is a pure-Slint style: no Qt needed on the machine.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint compile");
}
