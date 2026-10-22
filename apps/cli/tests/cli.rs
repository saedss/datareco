use std::path::PathBuf;
use std::process::{Command, Output};

fn datareco(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_datareco"))
        .args(args)
        .output()
        .expect("failed to run datareco")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// A two-sector image whose second sector starts with "HELLO".
fn hello_image(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    let mut bytes = vec![0u8; 1024];
    bytes[512..517].copy_from_slice(b"HELLO");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn version_flag_prints_name_and_version() {
    let out = datareco(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        format!("datareco {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_shows_help() {
    let out = datareco(&[]);
    let text = stderr(&out) + &stdout(&out);
    assert!(
        text.contains("Usage: datareco"),
        "unexpected output: {text}"
    );
}

#[test]
fn read_shows_a_sector_of_an_image_as_hex() {
    let image = hello_image("read.img");
    let out = datareco(&["read", image.to_str().unwrap(), "--sector", "1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("2 sectors of 512 bytes"), "{text}");
    assert!(text.contains("Sector 1"), "{text}");
    assert!(
        text.contains(
            "000000000200  48 45 4c 4c 4f 00 00 00  00 00 00 00 00 00 00 00  |HELLO...........|"
        ),
        "{text}"
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn reading_past_the_end_fails_with_a_clear_message() {
    let image = hello_image("past-end.img");
    let out = datareco(&["read", image.to_str().unwrap(), "--sector", "2"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("only 1024 bytes long"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn reading_a_missing_image_says_so() {
    let out = datareco(&["read", "no-such-image.img"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no-such-image.img was not found"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn disks_lists_devices_as_json() {
    let out = datareco(&["disks", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let devices: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(devices["disks"].is_array());
    assert!(devices["volumes"].is_array());
}
