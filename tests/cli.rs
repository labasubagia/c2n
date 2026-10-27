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
        .stdout(predicate::str::contains("upload-to-notion"))
        .stdout(predicate::str::contains("account-status"))
        .stdout(predicate::str::contains("delete-library"));
}

#[test]
fn rejects_conflicting_verbosity_before_loading_config() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args(["upload-to-notion", "--verbose", "--quiet"])
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
        .args(["upload-to-notion", "--all"])
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
        .args(["delete-library", "   ", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "QUERY must not be blank; omit it to target all images",
        ));
}

#[test]
fn rejects_conflicting_upload_flags() {
    Command::cargo_bin("c2n")
        .unwrap()
        .args([
            "upload-to-notion",
            "--upload-to-notion",
            "--no-upload-to-notion",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}
