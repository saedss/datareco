use std::process::Command;

fn datareco(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_datareco"))
        .args(args)
        .output()
        .expect("failed to run datareco")
}

#[test]
fn version_flag_prints_name_and_version() {
    let out = datareco(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("datareco {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_shows_help() {
    let out = datareco(&[]);
    let text = String::from_utf8(out.stderr).unwrap() + &String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("Usage: datareco"),
        "unexpected output: {text}"
    );
}
