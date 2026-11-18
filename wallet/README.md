# MuSig Wallet module

Wallet module of the Bisq-musig protocol. This crate has the primitives for interacting with a wallet.

It currently provides the following features:

- `ProtocolWalletApi`: the trait through which the `protocol` crate uses a wallet, implemented by a plain
  `bdk_wallet::Wallet` and the persistent `BMPWallet`
- `BMPWallet`: a persistent HD wallet on top of BDK
  - always password-protected: stored in an encrypted SQLite database (SQLCipher) whose key is derived from the wallet
    password with Argon2; the salt is stored next to the database as `<dbname>.salt`
  - the wallet password is a `Password` (`wallet::password`), which can only be built from a string that follows the
    password rules (at least 8 characters, with a lowercase letter, an uppercase letter, a number and a special
    character; a letter of a script without case, such as Chinese or Arabic, counts as both lowercase and uppercase);
    a rejected password's error states the rules in plain English
  - the wallet password can be checked and changed, but not removed
  - importing of external private keys (e.g. for the swap tx), which are signed with alongside the HD keys
  - sending to an address, listing addresses, UTXOs and transactions
- Syncing using Compact Block Filters (CBF), behind the `ChainDataSource` trait. The implementation is the
  long-running client `chain::client::ChainClient`, which the trade engine uses as well: a sync registers the scripts
  and the outputs of the wallets with the client, waits until the client has evaluated all blocks up to the chain tip
  for them, and applies what the client found. The first sync starts the client. A sync fails with an error while no
  peer is reachable, or while the peer serves no compact block filters.

## Imported-key identity

An imported key is identified by its internal Taproot key `P`, independently of its output
template. Therefore `tr(P)` and `tr(P, tap_tree)` represent the same imported key: once either
form is imported, another import with the same `P` is a duplicate and must not create a second
wallet entry.

## Derivation paths

The HD wallet follows BIP86: receive addresses are derived at `m/86'/coin_type'/0'/0/*` and change
addresses at `m/86'/coin_type'/0'/1/*`.

The path `m/86'/coin_type'/0'/2'` is reserved for the key shares of the trade protocol. The wallet
must never use it for an address. The rule for deriving the key shares, with test vectors, is
specified in
[docs/specifications/trade/key-derivation.md](../docs/specifications/trade/key-derivation.md). It
is implemented in `src/key_shares.rs`.

## Functions for trades

What the wallet does for the trade engine is specified in
[docs/specifications/wallet](../docs/specifications/wallet/README.md): keys and addresses of
trades, reservation of coins, recording of transactions, imported keys of payouts and the drain.

# Running tests

`cargo test -p wallet`

The tests spin up `bitcoind` via the [`testenv`](../testenv/README.md) crate.

# Running integration tests

In order to run the integration tests:

`cargo test -p wallet --test wallet_integration_test`

If you want to display the tests stdout, add the option `--show-output` as follows:

`cargo test -p wallet --test wallet_integration_test -- --show-output`
