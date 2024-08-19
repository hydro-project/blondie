use std::process::Command;

#[test]
fn test_simple() {
    let mut cmd = Command::new("ping");
    cmd.arg("localhost");
    let results = blondie::trace_command(cmd, false).unwrap();
    assert!(0 < results.iter_callstacks().count());
}
