use clap::Parser;

fn main() {
    let cmd = rps::Cmd::parse();
    env_logger::Builder::new()
        .filter_level(cmd.args.log_level)
        .format_target(false)
        .format_timestamp(None)
        .init();
    match cmd.subcmd {
        None => async_std::task::Builder::new()
            .name("server".to_string())
            .blocking(rps::server(cmd.args, None)),
        Some(rps::Subcmd::Test(args)) => async_std::task::Builder::new()
            .name("test".to_string())
            .blocking(rps::test::test(cmd.args, args)),
    }
}
