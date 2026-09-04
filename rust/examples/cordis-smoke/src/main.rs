//! YAML mounts a provider then a consumer; `--dump-config` prints the tree.

use dsh_cordis::{Context, Service};
use dsh_cordis_loader::{parse_entry_list, Loader};
use std::env;
use std::sync::Arc;

struct Ping;
impl Service for Ping {
    const KEY: &'static str = "ping";
}

fn main() {
    let yaml = r#"
- id: ping
  name: dsh-ping
- id: pong
  name: dsh-pong
"#;
    let entries = parse_entry_list(yaml).expect("yaml");
    if env::args().any(|arg| arg == "--dump-config") {
        print!("{}", Loader::dump_config(&entries));
        return;
    }
    let loader = Loader::new();
    loader.register("dsh-ping", |ctx, _| ctx.provide(Arc::new(Ping)));
    loader.register("dsh-pong", |ctx, _| {
        if !ctx.has_service("ping") {
            return Err(dsh_cordis::CordisError::plugin("ping missing"));
        }
        Ok(())
    });
    let ctx = Context::new();
    loader.mount(&ctx, &entries).expect("mount");
    assert!(ctx.has_service("ping"));
    println!("ok");
}
