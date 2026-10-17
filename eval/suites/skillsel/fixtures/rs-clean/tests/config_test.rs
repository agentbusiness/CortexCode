use appcfg::config::{parse_config, Config};

#[test]
fn example_file_parses() {
    let text = include_str!("../app.conf");
    let cfg = parse_config(text).expect("example config");
    assert_eq!(cfg, Config { name: "demo".into(), port: 8080, retries: 3 });
}
