# tests
This is a port of Android's test cases to the rsbinder environment.

There are a total of 96 test cases, of which 90 are currently in a passed state.
The 6 failed ones require the development of new features.

- [ ] test_vintf_parcelable_holder_cannot_contain_unstable_parcelable
- [ ] test_vintf_parcelable_holder_cannot_contain_not_vintf_parcelable
- [ ] test_versioned_unknown_union_field_triggers_error
- [ ] test_renamed_interface_new_as_new
- [ ] test_renamed_interface_old_as_new
- [ ] test_renamed_interface_new_as_old

## How to run test cases

* Run **rsb_hub** in a terminal
```
$ cargo run --bin rsb_hub
```

* Run **test_service** in another terminal
```
$ cargo run --bin test_service
```

* Run test cases in another terminal
```
$ cargo test test_client::
```
