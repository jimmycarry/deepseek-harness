use dsh_cordis_loader::{parse_entry_list, Loader};

#[test]
fn dump_config_lists_both_rows() {
    let yaml = r#"
- id: ping
  name: dsh-ping
- id: pong
  name: dsh-pong
"#;
    let dump = Loader::dump_config(&parse_entry_list(yaml).unwrap());
    assert!(dump.contains("dsh-ping"));
    assert!(dump.contains("dsh-pong"));
}
