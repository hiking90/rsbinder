# tests
This is a port of Android's test cases to the rsbinder environment.

Three cases need features rsbinder does not have yet, and CI skips them by
name:

- [ ] test_vintf_parcelable_holder_cannot_contain_unstable_parcelable
- [ ] test_vintf_parcelable_holder_cannot_contain_not_vintf_parcelable
- [ ] test_versioned_unknown_union_field_triggers_error

## Access-control gates (Linux only)

`rsb_hub`'s policy engine is exercised by `tests/scripts/run_hub_policy_ac.sh`,
which drives `ac61_probe` against a live hub over real kernel binder — a denial
only means anything when the caller uid comes from the binder driver. It starts
and stops its own hub, so run it with nothing else holding handle 0:

```
$ cargo build --bin rsb_hub -p rsbinder-tools && cargo build -p tests --bin ac61_probe
$ ./tests/scripts/run_hub_policy_ac.sh
```

## How to run the main suite

* Run **rsb_hub** in a terminal. The suite invents service names at run time,
  so point it at the permissive test policy:
```
$ cargo run --bin rsb_hub -- --config tests/policy/permissive.toml
```

* Run **test_service** in another terminal
```
$ cargo run --bin test_service
```

* Run test cases in another terminal
```
$ cargo test test_client::
```
