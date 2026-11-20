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

### UC3: happy trade with no wallet close

Alice and Bob are doing a trade. Alice sells Bitcoin worth 0.1 BTC to Bob.

### UC4: happy trade with swap

Alice and Bob are doing a trade. Alice sells Bitcoin worth 0.1 BTC to Bob.
Alice uses the SwapTx.

### UC5: custom trade

Alice and Bob are doing a trade. Alice sells Bitcoin worth 0.2 BTC to Bob.
Alice and Bob agree on a custom payout instead of closing the trade normally.
Alice gets 0.15 BTC, Bob gets 0.05 BTC, both minus their fees.

### UC6: trade claim

Alice and Bob are doing a trade. Alice as Maker sells Bitcoin worth 0.2 BTC to Bob.
After successfully making the DepositTx, Alice stops responding.

### UC7: trade redirect

Alice and Bob are doing a trade. Alice as Maker sells Bitcoin worth 0.2 BTC to Bob.
After successfully making the DepositTx, Bob issues the warning transaction. Then Alice issues the Redirect transaction.

### UC8: trade penalty

Alice and Bob are doing a trade. Alice as Maker sells Bitcoin worth 0.2 BTC to Bob.
Alice and Bob finish the trade successfully. But now Bob issues the warning transaction.

## Tests

Tests run on regtest.

Almost all Tests will need a similar setup. So we need a method to create a setup consisting of:
for Alice: one musigd instance with connection to a bitcoind and a wallet in her own directory
for Bib: same for Bob
The two bitcoind need to be peers to each other in the sense to use Compact Block Filters.

Both wallets need to be funded with 1 BTC.

For the tests that have timelocks, adavance the blcokchain with new mined blocks quickly,so the test times are not too
long.

### BT1: Blackbox test 1:

Precondition:  setup from above

Test UC1.

Postcondition:
Check that the transaction was mined
Check if Alice has 0.1 and the fees less in her wallet.
Check if Bob has 0.1 BTC more in his wallet.

### BT2: happy trade straight forward

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).

Test UC2.

Postconditions:

- Check that all coins of Alica and Bob are under the HD Wallet (their main wallet, nothing left in imported walet(s)).
- Check that Bob has 0.1 BTC more in the wallet and Alice less, minus the fees.

### BT3: happy trade no wallet close

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).

Test UC3.

Postconditions:

- Check that Bob has 0.1 BTC more in the wallet and Alice less, minus the fees.
- Cheeck that Alice and Bob have an imported wallet
- make sure Alice and Bob have the key to spend the output of the trade outputs by signing a (random) message with the
  key and profing the signatures correctness with the pubkey read from the blockchain.

### BT4: happy trade swap

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).

Test UC4.
Send Bob's trade output to his main wallet (must be ba using the adaptor signature of the SwapTx). Bob needs to find the
SwapTx on the chain using CBF.

Postconditions:

- Check that Bob has 0.1 BTC more in the wallet and Alice less, minus the fees.
- show that the adaptor signature was used to retrieve the private key for Bobs trade output. prove that that found key
  is actually the private key for his trade output.

### BT5: custom payout

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).
- fee shall be 10sats/vbyte

Test UC5.

Postconditions:

- check that Alice has 0.95 BTC in the wallet and Bob 1.05 BTC minus fees.
- check that the right amounts are on chain, read the CustomPayoutTx and check the outputs. 

### BT6: trade claim

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).
- fee shall be 10sats/vbyte

Test UC6.

Postconditions:

- check that Bob sent the WarningTx and ClaimTx
- check that Bob has 1.215 BTC in his wallet minus fees.
- check that warning time blocks+ claim lock time has passed since the DepositTx.

Defects found (not fixed)

The first BT6 version used the default lock times and mined each lock time in one call, as the plan suggests.

1. The chain client can stop following the chain. After several hundred blocks at once, the compact block filter client
   of musigd stayed below the chain tip in two runs, without an error and without a
   reconnect. The claim was never sent. The cause is not established; my hypothesis is a race in the filter library when
   the tip moves during a sync. Whether this can happen on a real network is not
   checked.
2. Requests then get no response. The wallet sync keeps the wallet locked while it waits, without a time limit, for the
   chain client to reach the tip. Every trade and wallet request waits for that
   lock. In one run SubscribeTradeEvents hung for 10 minutes and I had to stop the test by hand. This cause is confirmed
   from the code.

### BT7: trade redirect

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).
- fee shall be 10sats/vbyte

Test UC7.

Postconditions:

- check that Bob sent the WarningTx and Alice has sent the RedirectTx.
- check that Bob has 0.99 BTC in his wallet minus fees.
- check that Alice has 0.785 BTC in his wallet minus fees.

### BT8: trade penalty

Preconditions

- setup
- Alice as maker and Bob as taker, and security deposits of 0.01 BTC (Bob) and 0.015 BTC (Alice).
- fee shall be 10sats/vbyte

Test UC8.

Postconditions:

- check that Bob sent the WarningTx and Alice has sent the PenaltyTx.
- check that Bob has 0.99 BTC in his wallet minus fees.
- check that Alice has 1.01 BTC in her wallet minus fees.
