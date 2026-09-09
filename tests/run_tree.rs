/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use clap::Parser;
    use lifeguard::commands::run_tree::RunTreeArgs;
    use lifeguard::commands::run_tree::run;
    use lifeguard::test_lib::populate_temp_dir;
    use serde_json::Value;

    #[test]
    fn test_run_tree_resolves_cli_site_packages() {
        let tmp = populate_temp_dir(&[
            ("proj/main.py", "import foo\n"),
            ("sp/foo/__init__.py", ""),
            ("sp/bar/__init__.py", ""),
        ]);
        let proj = tmp.path().join("proj");
        let sp = tmp.path().join("sp");
        let output = tmp.path().join("out.json");

        let args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--site-packages",
            sp.to_str().unwrap(),
            "--sorted-output",
        ])
        .unwrap();
        run(args).unwrap();

        let content = fs::read_to_string(&output).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        let modules: BTreeSet<&str> = value["LAZY_ELIGIBLE"]
            .as_object()
            .expect("LAZY_ELIGIBLE object in output JSON")
            .keys()
            .map(|s| s.as_str())
            .collect();
        // `foo` appears only because --site-packages caused its resolution.
        // `bar` doesn't.
        assert_eq!(modules, BTreeSet::from(["main", "foo"]));
    }

    #[test]
    fn test_fix_py314_matches_golden_is_idempotent_and_dry_run_is_non_mutating() {
        let original = include_str!("e2e/fixtures/test_fixer.py");
        let expected = include_str!("e2e/fixtures/golden_test_fixer_py314.py");
        let other = include_str!("e2e/fixtures/other.py");
        let tmp = populate_temp_dir(&[("proj/main.py", original), ("proj/other.py", other)]);
        let proj = tmp.path().join("proj");
        let output = tmp.path().join("out.json");

        let fixed_args = || {
            RunTreeArgs::try_parse_from([
                "run-tree",
                proj.to_str().unwrap(),
                output.to_str().unwrap(),
                "--fix",
                "--target-version",
                "3.14",
            ])
            .unwrap()
        };
        run(fixed_args()).unwrap();
        let main = proj.join("main.py");
        assert_eq!(fs::read_to_string(&main).unwrap(), expected);

        // An existing declaration must be left alone on subsequent runs.
        run(fixed_args()).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), expected);

        fs::write(&main, original).unwrap();
        let dry_run_args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--dry-run",
            "--target-version",
            "3.15",
        ])
        .unwrap();
        run(dry_run_args).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), original);

        fs::write(&main, original).unwrap();
        let dry_run_args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--dry-run",
            "--target-version",
            "3.14",
        ])
        .unwrap();
        run(dry_run_args).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), original);
    }

    #[test]
    fn test_fix_py315_matches_golden_and_is_idempotent() {
        let original = include_str!("e2e/fixtures/test_fixer_py315.py");
        let expected = include_str!("e2e/fixtures/golden_test_fixer_py315.py");
        let other = include_str!("e2e/fixtures/other.py");
        let tmp = populate_temp_dir(&[("proj/main.py", original), ("proj/other.py", other)]);
        let proj = tmp.path().join("proj");
        let output = tmp.path().join("out.json");

        let fixed_args = || {
            RunTreeArgs::try_parse_from([
                "run-tree",
                proj.to_str().unwrap(),
                output.to_str().unwrap(),
                "--fix",
                "--target-version",
                "3.15",
            ])
            .unwrap()
        };
        run(fixed_args()).unwrap();
        let main = proj.join("main.py");
        assert_eq!(fs::read_to_string(&main).unwrap(), expected);

        run(fixed_args()).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), expected);
    }

    #[test]
    fn test_fix_py315_skips_mixed_eligibility_import_statements() {
        let source = "import safe, unsafe\n";
        let tmp = populate_temp_dir(&[
            ("proj/main.py", source),
            ("proj/safe.py", "value = 1\n"),
            // `exec` is a Lifeguard load-imports-eagerly trigger, so unlike
            // `safe`, `unsafe` is absent from `lazy_eligible`.
            ("proj/unsafe.py", "exec('value = 1')\n"),
        ]);
        let proj = tmp.path().join("proj");
        let output = tmp.path().join("out.json");
        let args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--fix",
            "--target-version",
            "3.15",
        ])
        .unwrap();

        run(args).unwrap();
        assert_eq!(fs::read_to_string(proj.join("main.py")).unwrap(), source);
    }

    #[test]
    fn test_fix_py315_handles_conditional_and_multiline_imports() {
        let source = "if True:\n    import other\n\nfrom package import (\n    member,\n)\n";
        let expected =
            "if True:\n    lazy import other\n\nlazy from package import (\n    member,\n)\n";
        let tmp = populate_temp_dir(&[
            ("proj/main.py", source),
            ("proj/other.py", "def value():\n    return 1\n"),
            ("proj/package/__init__.py", ""),
            ("proj/package/member.py", "value = 1\n"),
        ]);
        let proj = tmp.path().join("proj");
        let output = tmp.path().join("out.json");
        let args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--fix",
            "--target-version",
            "3.15",
        ])
        .unwrap();

        run(args).unwrap();
        assert_eq!(fs::read_to_string(proj.join("main.py")).unwrap(), expected);
    }
}
