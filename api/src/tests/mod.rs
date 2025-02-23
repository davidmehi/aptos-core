// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

mod accounts_test;
mod events_test;
mod golden_output;
mod index_test;
mod invalid_post_request_test;
mod string_resource_test;
mod test_context;
mod transactions_test;

use serde_json::Value;
pub use test_context::{new_test_context, TestContext};

pub fn find_value(val: &Value, filter: for<'r> fn(&'r &Value) -> bool) -> Value {
    let resources = val
        .as_array()
        .unwrap_or_else(|| panic!("expect array, but got: {}", val));
    let mut balances = resources.iter().filter(filter);
    match balances.next() {
        Some(resource) => {
            let more = balances.next();
            if let Some(val) = more {
                panic!("found multiple items by the filter: {}", pretty(val));
            }
            resource.clone()
        }
        None => {
            panic!("\ncould not find item in {}", pretty(val))
        }
    }
}

pub fn assert_json(ret: Value, expected: Value) {
    assert!(
        ret == expected,
        "\nexpected: {}, \nbut got: {}",
        pretty(&expected),
        pretty(&ret)
    )
}

pub fn pretty(val: &Value) -> String {
    serde_json::to_string_pretty(val).unwrap() + "\n"
}

/// Returns the name of the current function. This macro is used to derive the name for the golden
/// file of each test case.
#[macro_export]
macro_rules! current_function_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let name = type_name_of(f);
        let mut strip = 3;
        if name.contains("::{{closure}}") {
            strip += 13;
        }
        &name[..name.len() - strip]
    }};
}
