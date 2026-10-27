//! Command-line validation tests that do not require network access.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_lists_core_commands() {
    Command::cargo_bin("c2n")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("img"))
        .stdout(predicate::str::contains("db"));
}

#[test]
fn img_help_lists_subcommands() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["img", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sync"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("clear"))
        .stdout(predicate::str::contains("library"));
}

#[test]
fn library_help_lists_delete_subcommand() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["img", "library", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("delete"));
}

#[test]
fn rejects_conflicting_verbosity_before_loading_config() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["img", "sync", "--verbose", "--quiet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Cannot use --verbose and --quiet together",
        ));
}

#[test]
fn rejects_all_without_history_mode_before_loading_config() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["img", "sync", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--all only applies with --from-history or --verify-history",
        ));
}

#[test]
fn rejects_blank_delete_library_query_before_loading_config() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["img", "library", "delete", "   ", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "QUERY must not be blank; omit it to target all images",
        ));
}

#[test]
fn old_commands_are_rejected() {
    Command::cargo_bin("c2n")
        .unwrap()
        .arg("upload-to-notion")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));

    Command::cargo_bin("c2n")
        .unwrap()
        .arg("download")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn creates_config_data_and_image_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = temp.path().join("cfg");
    let data = temp.path().join("data");
    Command::cargo_bin("c2n")
        .unwrap()
        .env("C2N_CONFIG_DIR", &cfg)
        .env("C2N_DATA_DIR", &data)
        .args(["img", "status"])
        .assert()
        .failure();
    assert!(cfg.is_dir());
    assert!(data.is_dir());
    assert!(data.join("images").is_dir());
}
