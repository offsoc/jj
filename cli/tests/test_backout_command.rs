// Copyright 2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::create_commit_with_files;
use crate::common::CommandOutput;
use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

#[test]
fn test_backout() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    create_commit_with_files(&work_dir, "a", &[], &[("a", "a\n")]);
    // Test the setup
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    ");
    let output = work_dir.run_jj(["diff", "-s"]);
    insta::assert_snapshot!(output, @r"
    A a
    [EOF]
    ");

    // Backout the commit
    let output = work_dir.run_jj(["backout", "-r", "@"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Warning: `jj backout` is deprecated; use `jj revert` instead
    Warning: `jj backout` will be removed in a future version, and this will be a hard error
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&work_dir), @r#"
    ○  6d845ed9fb6a Back out "a"
    │
    │  This backs out commit 2443ea76b0b1c531326908326aab7020abab8e6c.
    @  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    "#);
    let output = work_dir.run_jj(["diff", "-s", "-r", "@+"]);
    insta::assert_snapshot!(output, @r"
    D a
    [EOF]
    ");

    // Backout the new backed-out commit
    work_dir.run_jj(["edit", "@+"]).success();
    let output = work_dir.run_jj(["backout", "-r", "@"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Warning: `jj backout` is deprecated; use `jj revert` instead
    Warning: `jj backout` will be removed in a future version, and this will be a hard error
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&work_dir), @r#"
    ○  79555ea9040b Back out "Back out "a""
    │
    │  This backs out commit 6d845ed9fb6a3d367e2d7068ef0256b1a10705a9.
    @  6d845ed9fb6a Back out "a"
    │
    │  This backs out commit 2443ea76b0b1c531326908326aab7020abab8e6c.
    ○  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    "#);
    let output = work_dir.run_jj(["diff", "-s", "-r", "@+"]);
    insta::assert_snapshot!(output, @r"
    A a
    [EOF]
    ");
}

#[test]
fn test_backout_multiple() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    create_commit_with_files(&work_dir, "a", &[], &[("a", "a\n")]);
    create_commit_with_files(&work_dir, "b", &["a"], &[("a", "a\nb\n")]);
    create_commit_with_files(&work_dir, "c", &["b"], &[("a", "a\nb\n"), ("b", "b\n")]);
    create_commit_with_files(&work_dir, "d", &["c"], &[]);
    create_commit_with_files(&work_dir, "e", &["d"], &[("a", "a\nb\nc\n")]);

    // Test the setup
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  208f8612074a e
    ○  ceeec03be46b d
    ○  413337bbd11f c
    ○  46cc97af6802 b
    ○  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    ");

    // Backout multiple commits
    let output = work_dir.run_jj(["backout", "-r", "b", "-r", "c", "-r", "e"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Warning: `jj backout` is deprecated; use `jj revert` instead
    Warning: `jj backout` will be removed in a future version, and this will be a hard error
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&work_dir), @r#"
    ○  6504c4ded177 Back out "b"
    │
    │  This backs out commit 46cc97af6802301d8db381386e8485ff3ff24ae6.
    ○  d31d42e0267f Back out "c"
    │
    │  This backs out commit 413337bbd11f7a6636c010d9e196acf801d8df2f.
    ○  8ff3fbc2ccb0 Back out "e"
    │
    │  This backs out commit 208f8612074af4c219d06568a8e1f04f2e80dc25.
    @  208f8612074a e
    ○  ceeec03be46b d
    ○  413337bbd11f c
    ○  46cc97af6802 b
    ○  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    "#);
    // View the output of each backed out commit
    let output = work_dir.run_jj(["show", "@+"]);
    insta::assert_snapshot!(output, @r#"
    Commit ID: 8ff3fbc2ccb0d66985f558c461d1643cebb4c7d6
    Change ID: wqnwkozpkustnxypnnntnykwrqrkrpvv
    Author   : Test User <test.user@example.com> (2001-02-03 08:05:19)
    Committer: Test User <test.user@example.com> (2001-02-03 08:05:19)

        Back out "e"

        This backs out commit 208f8612074af4c219d06568a8e1f04f2e80dc25.

    Modified regular file a:
       1    1: a
       2    2: b
       3     : c
    [EOF]
    "#);
    let output = work_dir.run_jj(["show", "@++"]);
    insta::assert_snapshot!(output, @r#"
    Commit ID: d31d42e0267f6524d445348b1dd00926c62a6b57
    Change ID: mouksmquosnpvwqrpsvvxtxpywpnxlss
    Author   : Test User <test.user@example.com> (2001-02-03 08:05:19)
    Committer: Test User <test.user@example.com> (2001-02-03 08:05:19)

        Back out "c"

        This backs out commit 413337bbd11f7a6636c010d9e196acf801d8df2f.

    Removed regular file b:
       1     : b
    [EOF]
    "#);
    let output = work_dir.run_jj(["show", "@+++"]);
    insta::assert_snapshot!(output, @r#"
    Commit ID: 6504c4ded177fba2334f76683d1aa643700d5073
    Change ID: tqvpomtpwrqsylrpsxknultrymmqxmxv
    Author   : Test User <test.user@example.com> (2001-02-03 08:05:19)
    Committer: Test User <test.user@example.com> (2001-02-03 08:05:19)

        Back out "b"

        This backs out commit 46cc97af6802301d8db381386e8485ff3ff24ae6.

    Modified regular file a:
       1    1: a
       2     : b
    [EOF]
    "#);
}

#[test]
fn test_backout_description_template() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    test_env.add_config(
        r#"
        [templates]
        backout_description = '''
        separate(" ",
          "Revert commit",
          commit_id.short(),
          '"' ++ description.first_line() ++ '"',
        )
        '''
        "#,
    );
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "a", &[], &[("a", "a\n")]);

    // Test the setup
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    ");
    let output = work_dir.run_jj(["diff", "-s"]);
    insta::assert_snapshot!(output, @r"
    A a
    [EOF]
    ");

    // Verify that message of backed out commit follows the template
    let output = work_dir.run_jj(["backout", "-r", "a"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Warning: `jj backout` is deprecated; use `jj revert` instead
    Warning: `jj backout` will be removed in a future version, and this will be a hard error
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&work_dir), @r#"
    ○  1db880a5204e Revert commit 2443ea76b0b1 "a"
    @  2443ea76b0b1 a
    ◆  000000000000
    [EOF]
    "#);
}

#[must_use]
fn get_log_output(work_dir: &TestWorkDir) -> CommandOutput {
    let template = r#"commit_id.short() ++ " " ++ description"#;
    work_dir.run_jj(["log", "-T", template])
}
