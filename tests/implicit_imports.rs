/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {

    use lifeguard::test_lib::*;

    const WALDO: &str = r#"
        import foo.qux.baz
    "#;
    const FOO_INIT: &str = r#"
    "#;
    const FOO_BAR: &str = r#"
        Bar = "Bar"
    "#;
    const FOO_QUX_BAZ: &str = r#"
        def fn(seconds):
            def wrap(f):
                return f
            return wrap
    "#;

    fn check_main(main: &str, implicit: Vec<&str>) {
        let modules = vec![
            ("__main__", main),
            ("waldo", WALDO),
            ("foo.__init__", FOO_INIT),
            ("foo.bar", FOO_BAR),
            ("foo.qux.baz", FOO_QUX_BAZ),
        ];
        check_errors_and_implicit_imports(modules, vec![("__main__", implicit)]);
    }

    /// Control: access stops at the submodule itself.
    #[test]
    fn test_attribute_access_stopping_at_submodule() {
        check_main(
            r#"
            import foo.bar
            import waldo

            BAZ = foo.qux.baz
        "#,
            vec!["foo.qux", "foo.qux.baz"],
        );
    }

    /// Control: plain call through the unimported submodule.
    #[test]
    fn test_attribute_call_on_unimported_submodule() {
        check_main(
            r#"
            import foo.bar
            import waldo

            VALUE = foo.qux.baz.fn(3600)
        "#,
            vec!["foo.qux", "foo.qux.baz"],
        );
    }

    /// Decorator on a module-level function. The decorator expression is not
    /// routed through the generic expression walk, so the chain must be
    /// recorded by `check_decorators` itself.
    #[test]
    fn test_decorator_on_module_level_function() {
        check_main(
            r#"
            import foo.bar
            import waldo

            @foo.qux.baz.fn(3600)
            def handler():
                pass
        "#,
            vec!["foo.qux.baz"],
        );
    }

    /// The mattermost shape: decorator on a method inside a class body. The
    /// class body executes at import time, so the chain must resolve then.
    #[test]
    fn test_decorator_on_method_in_class_body() {
        check_main(
            r#"
            import foo.bar
            import waldo

            class Handler:
                @foo.qux.baz.fn(3600)
                def run(self):
                    pass
        "#,
            vec!["foo.qux.baz"],
        );
    }
}
