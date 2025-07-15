# `arti keys-raw`

`arti keys-raw` is a plumbing, unsafe command line utility for managing the
content of keystores.

**Important**: The functionalities of this subcommand operate at a lower
level than the `arti keys` commands, on individual keystore entries.
As such, misusing them can lead to unexpected behaviour. For example, using
`remove-by-path` to remove an identity key will trigger the generation of a
new identity key when the service is launched.
Before using `keys raw` subcommands, check whether your use case is covered
by the `arti keys` subcommands.

Like the other `arti` subcommands, it has an optional `--config` option for
specifying the TOML configuration file. Using the correct configuration file is
important, because the keystores that `arti keys-raw` interacts with are relative to the
state directory, which you might have overridden in the configuration.

> `arti keys-raw` is an experimental subcommand.
> To use it, you will need to compile `arti` with the experimental `onion-service-cli-extra` feature.


## Remove a keystore entry by path

`arti keys raw-remove-by-path` allows the operator to remove a single keystore entry, provided a path.
This command provides a way of removing not just recognized keys, but also unrecognized keys and paths.

```ignore
$ arti keys-raw remove-by-path [OPTIONS] --keystore-id <KEYSTORE_ID> <PATH>
```

The command takes an optional flag and a positional argument: `-k <KEYSTORE_ID>` (`--keystore-id`), which
represents the identifier of the keystore the entry should be removed from; and `<PATH>`, the
location of said entry.
If `-k` is omitted, the primary keystore ("arti") will be used.
The location must be of the form `<ARTI_PATH>.<ENTRY_TYPE>`, where `<ARTI_PATH>` is the
`ArtiPath` of the key, and `<ENTRY_TYPE>` is the string representation of its `KeystoreItemType`.
The values that need to be provided can be obtained using the `arti keys list` command.

```ignore
$ arti keys-raw remove-by-path -k arti hss/allium-cepa/ks_hs_id.ed25519_expanded_private
```

> `remove-by-path` doesn't support removing CTor keystore entries.
> `keys-raw remove-by-path` is currently very similar to `rm(1)` (note: the direct use of `rm`
> on keystores is allowed). In the future, however, support for non-on-disk keystores will
> be added, allowing the operator to interact with such keystores through the same interface.
