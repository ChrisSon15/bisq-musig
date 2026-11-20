# Backbox Testing

Blackbox testing shall test the bisq-musig project on the Rust side only. It shall use the API of the different crates
only to
simulate typical uses of the application, so called use cases. We assume 2 users by the name of Alice and Bob.

## use cases

Note: the setup of the systems is described somewhere else. This sections describes how Alice and Bob may use the
systems

### UC1: simple transaction

Alice sends Bob 0.1 BTC

### UC2: happy trade

Alice and Bob are doing a trade. Alice sells Bitcoin worth 0.1 BTC to Bob.
After the trade they both close the app.

## Tests

Tests run on regtest.

Almost all Tests will need a similar setup. So we need a method to create a setup consisting of:
for Alice: one musigd instance with connection to a bitcoind and a wallet in her own directory
for Bib: same for Bob
The two bitcoind need to be peers to each other in the sense to use Compact Block Filters.

Both wallets need to be funded with 1 BTC

### BT1: Blackbox test 1:

Precondition:  setup from above

Test UC1.

Postcondition:
Check that the transaction was mined
Check if Alice has 0.1 less in her wallet.
Check if Bob has 0.1 BTC more in his wallet.


