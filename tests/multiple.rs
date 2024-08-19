use std::process::Command;

#[test]
fn test_multiple() {
    let handle = std::thread::spawn(|| {
        let mut cmd = Command::new("ping");
        cmd.arg("localhost");
        let results = blondie::trace_command(cmd, false).unwrap();
        assert!(0 < results.iter_callstacks().count());
    });

    let mut cmd = Command::new("ping");
    cmd.arg("localhost");
    let results = blondie::trace_command(cmd, false).unwrap();
    assert!(0 < results.iter_callstacks().count());

    handle.join().unwrap();
}
