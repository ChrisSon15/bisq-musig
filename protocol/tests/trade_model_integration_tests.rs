//! Tests of the trade engine with real wallets and a regtest chain.
//!
//! Two trade models, one per trader, exchange their data directly. Each trader has a funded
//! wallet, which syncs over compact block filters with the node of the test as its peer. The
//! engine itself has no chain access, so the tests send its transactions to the node through
//! the RPC interface and mine the blocks.
//!
//! The tests are the scenarios of the former tests of `BMPProtocol`. The test IDs in the comments
//! refer to section 9.4 of `concept/CompletionPlan.md`.

use std::path::PathBuf;
use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::RpcApi as _;
use bdk_wallet::bitcoin::address::NetworkUnchecked;
use bdk_wallet::bitcoin::{
    Amount, BlockHash, FeeRate, Network, OutPoint, Transaction, Txid, XOnlyPublicKey,
};
use chain::client::{ChainClient, TrustedPeer};
use musig2::secp::Scalar;
use protocol::params::{EngineConfig, TradeParams};
use protocol::receiver::Receiver;
use protocol::script_paths::{deposit_payout_descriptor, deposit_payout_tap_tree};
use protocol::state::{ClosureType, TradeState};
use protocol::storage::ByVal;
use protocol::trade_model::{
    ExchangedAddresses, ExchangedKeys, ExchangedNonces, ExchangedSigs, Role, TradeModel,
    UndecodablePeerData,
};
use testenv::TestEnv;
use tokio::runtime::Runtime;
use wallet::bmp_wallet::{BMPWallet, WalletApi as _};
use wallet::password::Password;
use wallet::protocol_wallet_api::ProtocolWalletApi;

const TRADE_AMOUNT: Amount = Amount::from_sat(5_000_000);
const BUYERS_SECURITY_DEPOSIT: Amount = Amount::from_sat(1_000_000);
const SELLERS_SECURITY_DEPOSIT: Amount = Amount::from_sat(1_200_000);
// 10 sat per vbyte.
const FEE_RATE: FeeRate = FeeRate::from_sat_per_kwu(2_500);
// Lock times in blocks. They are short, so that the tests mine only a few blocks.
const WARNING_LOCK_TIME: u16 = 5;
const CLAIM_LOCK_TIME: u16 = 3;

/// The configuration of both engines: the lock times of the tests, a deposit timeout of 6 blocks
/// and a release safety margin of 1 block.
fn config() -> EngineConfig {
    EngineConfig::new(WARNING_LOCK_TIME, CLAIM_LOCK_TIME, 6, 1).expect("valid configuration")
}

// Weights of the signed transactions, as defined in `protocol::transaction`.
const WARNING_TX_WEIGHT: u64 = 846;
const FORWARDING_TX_WEIGHT: u64 = 444;
const ANCHOR_AMOUNT: Amount = Amount::from_sat(330);

/// The amount that each wallet of the tests is funded with.
const FUNDING: Amount = Amount::from_sat(10_0000_0000);

/// A password that follows the wallet's password rules; the test wallets are throwaway.
fn test_password() -> Password {
    "Test-Passw0rd"
        .parse()
        .expect("the test password follows the password rules")
}

/// The chain access of the wallets of a test: the long-running compact block filter client, with
/// the node of the test as its only peer, and the runtime that it runs on. This is the chain
/// access of the wallet in operation (D2). The first sync starts the client.
struct Chain {
    runtime: Runtime,
    client: ChainClient,
}

impl Chain {
    fn new(env: &TestEnv) -> Self {
        let peer = env.p2p_socket_addr().expect("peer address of the node");
        Self {
            runtime: Runtime::new().expect("create runtime"),
            client: ChainClient::new(Network::Regtest, vec![TrustedPeer::from_socket_addr(peer)]),
        }
    }

    /// Runs the chain sync of a wallet, when the client has evaluated all blocks of the node. A
    /// client that runs learns of a new block a moment after the node has mined it.
    fn sync(&self, env: &TestEnv, wallet: &mut BMPWallet) {
        let height = u32::try_from(env.block_count().expect("block count")).expect("small height");
        self.runtime
            .block_on(async {
                if self.client.is_started() {
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        self.client.wait_until_synced_to(height),
                    )
                    .await
                    .expect("the client reaches the chain tip in time")
                    .expect("the client runs");
                }
                wallet.sync_all(&self.client).await
            })
            .expect("the wallet syncs");
    }
}

/// Sends a transaction to the node through its RPC interface, which only the test code uses.
/// Returns the reject reason if the node refuses the transaction.
fn send_to_node(env: &TestEnv, tx: &Transaction) -> Result<Txid, String> {
    env.bitcoin_core_rpc_client()
        .expect("RPC client")
        .send_raw_transaction(tx)
        .map_err(|e| format!("{e:?}"))
}

/// Returns a wallet with one confirmed coin of the amount `funding`, and its directory.
fn funded_wallet(env: &mut TestEnv, funding: Amount) -> (BMPWallet, PathBuf) {
    let path = env.new_temp_path().to_path_buf();
    let mut wallet =
        BMPWallet::new(path.as_path().into(), &test_password(), Network::Regtest).unwrap();

    fund(env, &mut wallet, funding);
    (wallet, path)
}

/// Sends a coin of the amount `funding` to the wallet, confirms it and syncs the wallet.
fn fund(env: &mut TestEnv, wallet: &mut BMPWallet, funding: Amount) {
    let address = wallet.get_new_address().unwrap();
    let txid = env.fund_address(&address.address, funding).unwrap();
    env.mine_block().unwrap();
    Chain::new(env).sync(env, wallet);
    assert!(
        wallet.list_transactions().iter().any(|tx| tx.tx_id == txid),
        "the wallet found the funding transaction"
    );
}

/// One trader: his trade model and his wallet.
struct Trader {
    model: TradeModel,
    wallet: BMPWallet,
    /// The directory of the wallet database, to load the wallet again.
    wallet_path: PathBuf,
}

impl Trader {
    fn new(env: &mut TestEnv, role: Role) -> Self {
        Self::with_funding(env, role, FUNDING)
    }

    fn with_funding(env: &mut TestEnv, role: Role, funding: Amount) -> Self {
        let (mut wallet, wallet_path) = funded_wallet(env, funding);
        let model = TradeModel::new("trade-1".to_owned(), role, &mut wallet)
            .expect("the trade can be created");
        Self {
            model,
            wallet,
            wallet_path,
        }
    }

    /// The coins that are reserved for the trade are locked in the wallet.
    #[track_caller]
    fn assert_coins_reserved(&self, reserved: bool) {
        let coins = self.model.reserved_coins();
        if reserved {
            assert!(!coins.is_empty());
            for coin in coins {
                assert!(self.wallet.is_utxo_locked(*coin), "{coin} is locked");
            }
        } else {
            assert!(coins.is_empty());
            assert_eq!(self.wallet.list_locked_utxos(), vec![]);
        }
    }

    /// The trade parameters of the tests.
    const fn params() -> TradeParams {
        TradeParams {
            trade_amount: TRADE_AMOUNT,
            buyers_security_deposit: BUYERS_SECURITY_DEPOSIT,
            sellers_security_deposit: SELLERS_SECURITY_DEPOSIT,
            deposit_tx_fee_rate: FEE_RATE,
            prepared_tx_fee_rate: FEE_RATE,
            trade_fee_receiver: None,
        }
    }

    fn sent_key_shares(&self) -> ExchangedKeys<'static, ByVal> {
        let key_shares = self.model.my_key_shares().expect("key shares exist");
        ExchangedKeys {
            buyer_payout: *key_shares.buyer_payout,
            seller_payout: *key_shares.seller_payout,
            multisig_script: *key_shares.multisig_script,
        }
    }

    fn receive_key_shares(&mut self, peer: &Self) {
        self.model
            .receive_key_shares(
                &mut self.wallet,
                &config(),
                Ok(peer.sent_key_shares()),
                Self::params(),
            )
            .expect("valid key shares and parameters");
    }

    fn receive_nonce_shares(&mut self, peer: &Self, receivers: &[Receiver<NetworkUnchecked>]) {
        let half_deposit_psbt = peer
            .model
            .my_half_deposit_psbt()
            .expect("PSBT exists")
            .clone();
        let addresses = peer.model.my_addresses().expect("addresses exist");
        let addresses = ExchangedAddresses {
            warning_tx_fee_bump: addresses.warning_tx_fee_bump.as_unchecked().clone(),
            redirect_tx_fee_bump: addresses.redirect_tx_fee_bump.as_unchecked().clone(),
            claim_tx_payout: addresses.claim_tx_payout.as_unchecked().clone(),
        };
        let nonces = peer.model.my_nonce_shares().expect("nonce shares exist");
        let nonces = ExchangedNonces {
            swap_tx_input: nonces.swap_tx_input.clone(),
            buyers_warning_tx_buyer_input: nonces.buyers_warning_tx_buyer_input.clone(),
            buyers_warning_tx_seller_input: nonces.buyers_warning_tx_seller_input.clone(),
            sellers_warning_tx_buyer_input: nonces.sellers_warning_tx_buyer_input.clone(),
            sellers_warning_tx_seller_input: nonces.sellers_warning_tx_seller_input.clone(),
            buyers_redirect_tx_input: nonces.buyers_redirect_tx_input.clone(),
            sellers_redirect_tx_input: nonces.sellers_redirect_tx_input.clone(),
            buyers_claim_tx_input: nonces.buyers_claim_tx_input.clone(),
            sellers_claim_tx_input: nonces.sellers_claim_tx_input.clone(),
        };
        self.model
            .receive_nonce_shares(
                Ok((half_deposit_psbt, addresses, nonces)),
                receivers.to_vec(),
            )
            .expect("valid nonce shares");
    }

    fn sent_partial_signatures(&self) -> ExchangedSigs<'static, ByVal> {
        let sigs = self
            .model
            .my_partial_signatures_on_peer_txs()
            .expect("signatures exist");
        ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature: *sigs
                .peers_warning_tx_buyer_input_partial_signature,
            peers_warning_tx_seller_input_partial_signature: *sigs
                .peers_warning_tx_seller_input_partial_signature,
            peers_redirect_tx_input_partial_signature: *sigs
                .peers_redirect_tx_input_partial_signature,
            peers_claim_tx_input_partial_signature: *sigs.peers_claim_tx_input_partial_signature,
            swap_tx_input_partial_signature: sigs.swap_tx_input_partial_signature.copied(),
            swap_tx_input_sighash: sigs.swap_tx_input_sighash.copied(),
            contractual_txids: None,
        }
    }

    fn receive_partial_signatures(
        &mut self,
        peers_partial_signatures: ExchangedSigs<'static, ByVal>,
    ) {
        self.model
            .receive_partial_signatures(&mut self.wallet, Ok(peers_partial_signatures))
            .expect("valid partial signatures");
    }
}

/// A trade of two traders with a confirmed deposit transaction.
struct Trade {
    env: TestEnv,
    chain: Chain,
    buyer: Trader,
    seller: Trader,
    buyer_payout: OutPoint,
    seller_payout: OutPoint,
    /// The only receiver of the redirect transactions.
    redirection_receiver: Receiver<NetworkUnchecked>,
}

impl Trade {
    /// Sets up a trade in which the buyer is the taker, publishes the deposit transaction and
    /// mines one block (part of A1).
    fn with_confirmed_deposit() -> anyhow::Result<Self> {
        let mut env = TestEnv::new()?;
        let mut buyer = Trader::new(&mut env, Role::BuyerAsTaker);
        let mut seller = Trader::new(&mut env, Role::SellerAsMaker);
        let chain = Chain::new(&env);

        seller.receive_key_shares(&buyer);
        buyer.receive_key_shares(&seller);

        // One receiver gets everything that a redirect transaction can distribute.
        let redirection_amount_msat = buyer.model.redirection_amount_msat()?;
        assert_eq!(
            redirection_amount_msat,
            seller.model.redirection_amount_msat()?
        );
        let receivers = Receiver::compute_receivers_from_shares(
            vec![(env.new_address()?, 1.0)],
            redirection_amount_msat,
            FEE_RATE,
        )
        .expect("the redirection amount is large enough for one receiver");
        let redirection_receiver = Receiver {
            address: receivers[0].address.as_unchecked().clone(),
            amount: receivers[0].amount,
        };

        buyer.receive_nonce_shares(&seller, std::slice::from_ref(&redirection_receiver));
        seller.receive_nonce_shares(&buyer, std::slice::from_ref(&redirection_receiver));

        let buyers_partial_signatures = buyer.sent_partial_signatures();
        let sellers_partial_signatures = seller.sent_partial_signatures();
        seller.receive_partial_signatures(buyers_partial_signatures);
        buyer.receive_partial_signatures(sellers_partial_signatures);
        assert_eq!(buyer.model.state(), TradeState::Deposit);
        assert_eq!(seller.model.state(), TradeState::Deposit);

        // The taker holds all signatures and publishes the deposit transaction.
        let sellers_deposit_psbt = seller.model.deposit_psbt()?.clone();
        let deposit_tx = buyer
            .model
            .receive_deposit_psbt(&mut buyer.wallet, Ok(sellers_deposit_psbt))?;
        let deposit_txid = send_to_node(&env, &deposit_tx).map_err(anyhow::Error::msg)?;
        env.mine_block()?;

        let payout = |amount: Amount| {
            let mut outputs = deposit_tx.output.iter().enumerate();
            let (vout, _) = outputs
                .find(|(_, output)| output.value == amount)
                .expect("the deposit transaction has an output with the amount");
            OutPoint::new(deposit_txid, u32::try_from(vout).expect("small index"))
        };
        let trade = Self {
            buyer_payout: payout(TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT),
            seller_payout: payout(SELLERS_SECURITY_DEPOSIT),
            env,
            chain,
            buyer,
            seller,
            redirection_receiver,
        };
        assert_eq!(trade.confirmations(trade.buyer_payout), Some(1));
        assert_eq!(trade.confirmations(trade.seller_payout), Some(1));
        Ok(trade)
    }

    /// Runs the chain sync of both wallets.
    fn sync_wallets(&mut self) {
        self.chain.sync(&self.env, &mut self.buyer.wallet);
        self.chain.sync(&self.env, &mut self.seller.wallet);
    }

    /// The number of confirmations of an unspent output, or `None` if the output does not exist
    /// or is spent by a confirmed transaction.
    fn confirmations(&self, outpoint: OutPoint) -> Option<u32> {
        self.unspent_output(outpoint)
            .map(|(_, confirmations)| confirmations)
    }

    /// The amount and the number of confirmations of an unspent output.
    fn unspent_output(&self, outpoint: OutPoint) -> Option<(Amount, u32)> {
        let rpc = self.env.bitcoin_core_rpc_client().expect("RPC client");
        rpc.get_tx_out(&outpoint.txid, outpoint.vout, Some(false))
            .expect("the node answers")
            .map(|output| (output.value, output.confirmations))
    }

    /// Sends a transaction to the node and returns the reject reason if the node refuses it.
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, String> {
        send_to_node(&self.env, tx)
    }

    /// Sends a transaction, mines a block and checks that the transaction is confirmed.
    fn broadcast_and_confirm(&mut self, tx: &Transaction) -> anyhow::Result<BlockHash> {
        let txid = self.broadcast(tx).map_err(anyhow::Error::msg)?;
        let block_hash = self.env.mine_block()?;
        assert_eq!(
            self.confirmations(OutPoint::new(txid, 0)),
            Some(1),
            "the transaction {txid} is confirmed"
        );
        Ok(block_hash)
    }

    /// Mines blocks until the deposit transaction has as many confirmations as the warning lock
    /// time, so that a warning transaction is valid.
    fn mine_until_warning_is_valid(&mut self) -> anyhow::Result<()> {
        let confirmations = self
            .confirmations(self.buyer_payout)
            .expect("deposit is unspent");
        self.env
            .mine_blocks((u32::from(WARNING_LOCK_TIME) - confirmations) as usize)?;
        Ok(())
    }

    /// Runs the cooperative close: messages E, F and G.
    fn close_cooperatively(&mut self) -> anyhow::Result<()> {
        self.start_payment_and_release()?;
        let sellers_key_share = *self.seller.model.my_private_key_share_for_peer_output()?;
        self.buyer.model.close_with_key_share(
            &mut self.buyer.wallet,
            Ok(sellers_key_share),
            None,
        )?;
        let buyers_key_share = *self.buyer.model.my_private_key_share_for_peer_output()?;
        self.seller.model.close_with_key_share(
            &mut self.seller.wallet,
            Ok(buyers_key_share),
            None,
        )?;
        let closed = TradeState::TradeClosed(ClosureType::Cooperative);
        assert_eq!(self.buyer.model.state(), closed);
        assert_eq!(self.seller.model.state(), closed);
        Ok(())
    }

    /// The buyer starts the payment (message E) and the seller confirms it. Returns the seller's
    /// key share of message F.
    fn start_payment_and_release(&mut self) -> anyhow::Result<Scalar> {
        self.buyer.model.start_payment()?;
        let swap_tx_signature = *self
            .buyer
            .model
            .my_partial_signatures_on_peer_txs()?
            .swap_tx_input_partial_signature
            .expect("message E contains the buyer's partial signature");
        self.seller
            .model
            .receive_swap_tx_signature(Ok(swap_tx_signature))?;
        Ok(self.seller.model.release_payout()?)
    }

    /// The escrow output of a warning transaction.
    fn escrow(warning_tx: &Transaction) -> OutPoint {
        OutPoint::new(warning_tx.compute_txid(), 0)
    }
}

/// The fee in sat for a weight at the fee rate of the tests, rounded up.
const fn fee(weight: u64) -> Amount {
    Amount::from_sat((FEE_RATE.to_sat_per_kwu() * weight).div_ceil(1000))
}

fn escrow_amount() -> Amount {
    TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT + SELLERS_SECURITY_DEPOSIT
        - ANCHOR_AMOUNT
        - fee(WARNING_TX_WEIGHT)
}

/// A1, as far as the engine goes without chain access: the deposit transaction of a complete setup
/// with real wallets confirms, and its payouts have the agreed amounts.
#[test]
fn a1_setup_and_deposit() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    // F13: the coins of both traders are reserved from the moment the half deposit exists.
    trade.buyer.assert_coins_reserved(true);
    trade.seller.assert_coins_reserved(true);
    // F14: the taker recorded the deposit transaction when he published it. His wallet knows
    // that its coin is spent, although it has not seen the block yet.
    let buyers_coin = trade.buyer.model.reserved_coins()[0];
    assert!(
        trade
            .buyer
            .wallet
            .list_utxos()
            .iter()
            .all(|utxo| OutPoint::new(utxo.tx_id, utxo.vout) != buyers_coin)
    );
    // After the sync both wallets hold their change, and no unspent coin is locked.
    trade.sync_wallets();
    for (trader, paid) in [
        (&trade.buyer, BUYERS_SECURITY_DEPOSIT),
        (&trade.seller, TRADE_AMOUNT + SELLERS_SECURITY_DEPOSIT),
    ] {
        assert_eq!(trader.wallet.list_locked_utxos(), vec![]);
        let balance = trader.wallet.balance();
        assert!(balance < FUNDING - paid, "{balance}");
        assert!(
            balance > FUNDING - paid - Amount::from_sat(10_000),
            "{balance}"
        );
    }
    assert_eq!(
        trade.unspent_output(trade.buyer_payout),
        Some((TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT, 1))
    );
    assert_eq!(
        trade.unspent_output(trade.seller_payout),
        Some((SELLERS_SECURITY_DEPOSIT, 1))
    );
    Ok(())
}

/// C1 and C2: the seller closes with the swap transaction. The buyer finds the spend of the
/// seller's payout in the block, reads the seller's key share from it and closes his trade.
#[test]
fn c1_c2_forced_close_with_swap_tx() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    trade.start_payment_and_release()?;

    trade.seller.model.close_by_force()?;
    assert_eq!(
        trade.seller.model.state(),
        TradeState::TradeClosed(ClosureType::Forced)
    );
    let swap_tx = trade.seller.model.signed_swap_tx()?.clone();
    let block_hash = trade.broadcast_and_confirm(&swap_tx)?;
    assert_eq!(
        trade.unspent_output(OutPoint::new(swap_tx.compute_txid(), 0)),
        Some((SELLERS_SECURITY_DEPOSIT - fee(FORWARDING_TX_WEIGHT), 1)),
        "the seller receives his deposit minus the fee"
    );
    assert_eq!(trade.confirmations(trade.seller_payout), None);

    // The buyer does not know the swap transaction. He looks for a transaction in the block that
    // spends the seller's payout.
    let block = trade
        .env
        .bitcoin_core_rpc_client()?
        .get_block(&block_hash)?;
    let spending_tx = block
        .txdata
        .iter()
        .find(|tx| {
            tx.input
                .iter()
                .any(|input| input.previous_output == trade.seller_payout)
        })
        .expect("the block contains a spend of the seller's payout");
    trade
        .buyer
        .model
        .close_with_swap_tx(&mut trade.buyer.wallet, spending_tx, None)?;
    assert_eq!(
        trade.buyer.model.state(),
        TradeState::TradeClosed(ClosureType::Forced)
    );
    // The buyer's payout stays in the deposit output. The buyer now holds its full key, and the
    // key is imported into his wallet (F5).
    assert_eq!(trade.confirmations(trade.buyer_payout), Some(2));
    trade.sync_wallets();
    assert_eq!(
        trade.buyer.wallet.imported_balance().confirmed,
        TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT
    );
    // The seller received his deposit through the swap transaction, to an address of his wallet.
    assert_eq!(trade.seller.wallet.imported_balance().total(), Amount::ZERO);
    let sellers_balance = trade.seller.wallet.balance();
    assert!(
        sellers_balance > FUNDING - TRADE_AMOUNT - Amount::from_sat(10_000),
        "{sellers_balance}"
    );
    Ok(())
}

/// A2 and H1: a trade is abandoned before the trader signed the deposit transaction. The reserved
/// coins are free again, and the trade cannot continue. After the deposit transaction is signed,
/// a trade cannot be abandoned.
#[test]
fn a2_abandon_releases_the_reserved_coins() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let mut buyer = Trader::new(&mut env, Role::BuyerAsTaker);
    let mut seller = Trader::new(&mut env, Role::SellerAsMaker);
    seller.receive_key_shares(&buyer);
    buyer.receive_key_shares(&seller);
    for trader in [&mut buyer, &mut seller] {
        trader.assert_coins_reserved(true);
        assert_eq!(trader.wallet.balance(), Amount::ZERO);
        assert_eq!(trader.wallet.locked_balance().total(), FUNDING);

        trader.model.abandon(&mut trader.wallet)?;
        trader.assert_coins_reserved(false);
        assert_eq!(trader.wallet.balance(), FUNDING);
        assert_eq!(
            trader.model.state(),
            TradeState::TradeClosed(ClosureType::Abandoned)
        );
        // The request can be repeated.
        trader.model.abandon(&mut trader.wallet)?;
    }
    // The abandoned trade does not accept the next message of the setup.
    let result = buyer
        .model
        .receive_nonce_shares(Err(UndecodablePeerData("unused".to_owned())), vec![]);
    assert!(result.expect_err("the trade is abandoned").is_refusal());

    // With a signed deposit transaction the coins stay reserved.
    let mut trade = Trade::with_confirmed_deposit()?;
    for trader in [&mut trade.buyer, &mut trade.seller] {
        let result = trader.model.abandon(&mut trader.wallet);
        assert!(result.expect_err("the deposit is signed").is_refusal());
        assert_eq!(trader.model.state(), TradeState::Deposit);
        trader.assert_coins_reserved(true);
    }
    Ok(())
}

/// A6: the wallet has too few funds for the deposit. The request fails, and nothing is reserved.
#[test]
fn a6_too_few_funds() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let mut buyer = Trader::with_funding(&mut env, Role::BuyerAsTaker, Amount::from_sat(500_000));
    let seller = Trader::new(&mut env, Role::SellerAsMaker);
    let result = buyer.model.receive_key_shares(
        &mut buyer.wallet,
        &config(),
        Ok(seller.sent_key_shares()),
        Trader::params(),
    );
    let error = result.expect_err("500000 sat do not pay a deposit of 1000000 sat");
    assert!(error.to_string().contains("Insufficient funds"), "{error}");
    buyer.assert_coins_reserved(false);
    assert_eq!(buyer.wallet.balance(), Amount::from_sat(500_000));
    assert_eq!(buyer.model.state(), TradeState::Init);
    assert!(buyer.model.my_half_deposit_psbt().is_err());
    Ok(())
}

/// A8 and I3: two trades from one wallet never use the same coin, a third trade finds no free
/// coin, and a payment during the open trades does not use a reserved coin.
#[test]
fn a8_i3_reserved_coins_are_used_once() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let peer = Trader::new(&mut env, Role::SellerAsMaker);
    let (mut wallet, _path) = funded_wallet(&mut env, FUNDING);
    fund(&mut env, &mut wallet, FUNDING);

    let start_trade = |wallet: &mut BMPWallet, trade_id: &str| {
        let mut model = TradeModel::new(trade_id.to_owned(), Role::BuyerAsTaker, wallet)?;
        model.receive_key_shares(
            wallet,
            &config(),
            Ok(peer.sent_key_shares()),
            Trader::params(),
        )?;
        anyhow::Ok(model)
    };
    let mut first = start_trade(&mut wallet, "trade-1")?;
    let second = start_trade(&mut wallet, "trade-2")?;
    assert_eq!(first.reserved_coins().len(), 1);
    assert_eq!(second.reserved_coins().len(), 1);
    assert_ne!(first.reserved_coins(), second.reserved_coins());
    assert_eq!(wallet.balance(), Amount::ZERO);

    // Both coins are reserved now.
    let error = start_trade(&mut wallet, "trade-3")
        .map(|_| ())
        .expect_err("no free coin");
    assert!(error.to_string().contains("Insufficient funds"), "{error}");
    let address = env.new_address()?;
    let payment = wallet.send_to_address(&address, Amount::from_sat(100_000), FEE_RATE);
    assert!(payment.is_err(), "a payment does not use a reserved coin");
    assert_eq!(wallet.list_locked_utxos().len(), 2);

    // When the first trade is abandoned, its coin can be used again.
    first.abandon(&mut wallet)?;
    let payment = wallet.send_to_address(&address, Amount::from_sat(100_000), FEE_RATE)?;
    assert_ne!(payment.input[0].previous_output, second.reserved_coins()[0]);
    Ok(())
}

/// I2, B2 and B6 for the wallet of one trader after the close: the imported key and its coin are
/// stored, so the payout is there after the wallet was closed and loaded again. The next payment
/// spends the payout first, signed with the imported key, and it confirms.
fn assert_payout_is_kept_and_spent_first(
    trader: Trader,
    payout: OutPoint,
    amount: Amount,
    env: &mut TestEnv,
    chain: &Chain,
) -> anyhow::Result<()> {
    let Trader {
        wallet,
        wallet_path,
        ..
    } = trader;
    let balance = wallet.balance();
    drop(wallet);
    let mut wallet = BMPWallet::load_wallet(
        wallet_path.as_path().into(),
        Network::Regtest,
        &test_password(),
    )?;
    assert_eq!(wallet.imported_balance().confirmed, amount);
    assert_eq!(wallet.balance(), balance);

    let address = env.new_address()?;
    let payment = wallet.send_to_address(&address, Amount::from_sat(100_000), FEE_RATE)?;
    assert!(
        payment
            .input
            .iter()
            .any(|input| input.previous_output == payout),
        "the payment spends the imported payout first"
    );
    send_to_node(env, &payment).map_err(anyhow::Error::msg)?;
    env.mine_block()?;
    chain.sync(env, &mut wallet);
    assert_eq!(wallet.imported_balance().total(), Amount::ZERO);
    let fee = Amount::from_sat(5_000);
    assert!(wallet.balance() > balance - Amount::from_sat(100_000) - fee);
    assert!(wallet.balance() < balance - Amount::from_sat(100_000));
    Ok(())
}

/// B1, B2, B6 and I2: after a cooperative close each wallet shows the payout of its trader. The
/// payout is still there after the wallet was closed and loaded again. The next payment of each
/// wallet spends the payout first, and it confirms.
#[test]
fn b1_b2_cooperative_close_and_payout_in_the_wallet() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    trade.close_cooperatively()?;
    // No transaction is needed for the cooperative close. The payouts stay in the deposit.
    trade.env.mine_block()?;
    assert_eq!(trade.confirmations(trade.buyer_payout), Some(2));
    assert_eq!(trade.confirmations(trade.seller_payout), Some(2));

    trade.sync_wallets();
    let buyers_payout = TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT;
    assert_eq!(
        trade.buyer.wallet.imported_balance().confirmed,
        buyers_payout
    );
    assert_eq!(
        trade.seller.wallet.imported_balance().confirmed,
        SELLERS_SECURITY_DEPOSIT
    );
    // The buyer gained the trade amount and the seller paid it, apart from the deposit fee.
    let fee_limit = Amount::from_sat(10_000);
    let buyers_balance = trade.buyer.wallet.balance();
    let sellers_balance = trade.seller.wallet.balance();
    assert!(buyers_balance <= FUNDING + TRADE_AMOUNT && sellers_balance <= FUNDING - TRADE_AMOUNT);
    assert!(
        buyers_balance > FUNDING + TRADE_AMOUNT - fee_limit,
        "{buyers_balance}"
    );
    assert!(
        sellers_balance > FUNDING - TRADE_AMOUNT - fee_limit,
        "{sellers_balance}"
    );

    let Trade {
        buyer,
        seller,
        chain,
        mut env,
        buyer_payout,
        seller_payout,
        ..
    } = trade;
    assert_payout_is_kept_and_spent_first(buyer, buyer_payout, buyers_payout, &mut env, &chain)?;
    assert_payout_is_kept_and_spent_first(
        seller,
        seller_payout,
        SELLERS_SECURITY_DEPOSIT,
        &mut env,
        &chain,
    )?;
    Ok(())
}

/// E1: the node rejects a warning transaction before its lock time has passed, and accepts it
/// afterwards.
#[test]
fn e1_warning_before_and_after_lock_time() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    let warning_tx = trade.seller.model.my_signed_warning_tx()?.clone();

    let reject_reason = trade
        .broadcast(&warning_tx)
        .expect_err("the warning lock time has not passed");
    assert!(reject_reason.contains("non-BIP68-final"), "{reject_reason}");

    // One block before the lock time ends, the node still rejects the transaction.
    trade.env.mine_blocks(usize::from(WARNING_LOCK_TIME) - 2)?;
    let reject_reason = trade
        .broadcast(&warning_tx)
        .expect_err("the warning lock time has not passed");
    assert!(reject_reason.contains("non-BIP68-final"), "{reject_reason}");

    trade.env.mine_block()?;
    trade.broadcast_and_confirm(&warning_tx)?;
    assert_eq!(
        trade.unspent_output(Trade::escrow(&warning_tx)),
        Some((escrow_amount(), 1))
    );
    assert_eq!(trade.confirmations(trade.buyer_payout), None);
    assert_eq!(trade.confirmations(trade.seller_payout), None);
    Ok(())
}

/// E2 and E14: after the warning transaction of the seller is confirmed, the buyer's redirect
/// transaction sends the funds to the redirection receiver. The node rejects the redirect
/// transaction while the warning transaction is unconfirmed (D4).
#[test]
fn e2_e14_redirect_after_peers_warning() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    trade.mine_until_warning_is_valid()?;
    let warning_tx = trade.seller.model.my_signed_warning_tx()?.clone();
    let redirect_tx = trade.buyer.model.my_signed_redirect_tx()?.clone();
    assert_eq!(
        redirect_tx.input[0].previous_output,
        Trade::escrow(&warning_tx)
    );

    trade.broadcast(&warning_tx).map_err(anyhow::Error::msg)?;
    let reject_reason = trade
        .broadcast(&redirect_tx)
        .expect_err("the warning transaction is not confirmed");
    assert!(reject_reason.contains("non-BIP68-final"), "{reject_reason}");

    trade.env.mine_block()?;
    trade.broadcast_and_confirm(&redirect_tx)?;
    assert_eq!(
        trade.unspent_output(OutPoint::new(redirect_tx.compute_txid(), 0)),
        Some((trade.redirection_receiver.amount, 1)),
        "the redirection receiver gets the funds"
    );
    assert_eq!(trade.confirmations(Trade::escrow(&warning_tx)), None);
    Ok(())
}

/// E3 and E4 with the pre-signed claim transaction: the node rejects the claim before the claim
/// lock time has passed, and accepts it afterwards. Decision D3 replaces the pre-signed claim by
/// a claim through the script path; this test then changes.
#[test]
fn e3_e4_claim_before_and_after_lock_time() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    trade.mine_until_warning_is_valid()?;
    let warning_tx = trade.buyer.model.my_signed_warning_tx()?.clone();
    let claim_tx = trade.buyer.model.my_signed_claim_tx()?.clone();
    trade.broadcast_and_confirm(&warning_tx)?;

    // One block before the lock time ends, the node still rejects the claim.
    trade.env.mine_blocks(usize::from(CLAIM_LOCK_TIME) - 2)?;
    let reject_reason = trade
        .broadcast(&claim_tx)
        .expect_err("the claim lock time has not passed");
    assert!(reject_reason.contains("non-BIP68-final"), "{reject_reason}");

    trade.env.mine_block()?;
    trade.broadcast_and_confirm(&claim_tx)?;
    assert_eq!(
        trade.unspent_output(OutPoint::new(claim_tx.compute_txid(), 0)),
        Some((escrow_amount() - fee(FORWARDING_TX_WEIGHT), 1)),
        "the publisher of the warning transaction gets all funds minus the fees"
    );
    Ok(())
}

/// F1 for both roles: after a cooperative close, one trader publishes his warning transaction.
/// The other trader takes the escrow with his penalty transaction. The penalty transaction is
/// signed with the aggregated key of the payout, so this also checks the exchanged key shares.
#[test]
fn f1_penalty_after_cooperative_close() -> anyhow::Result<()> {
    for cheater_is_buyer in [false, true] {
        let mut trade = Trade::with_confirmed_deposit()?;
        trade.close_cooperatively()?;
        trade.mine_until_warning_is_valid()?;

        let (cheater, victim) = if cheater_is_buyer {
            (&trade.buyer, &trade.seller)
        } else {
            (&trade.seller, &trade.buyer)
        };
        let warning_tx = cheater.model.my_signed_warning_tx()?.clone();
        let penalty_tx = victim.model.signed_penalty_tx()?.clone();
        assert_eq!(
            penalty_tx.input[0].previous_output,
            Trade::escrow(&warning_tx)
        );

        trade.broadcast_and_confirm(&warning_tx)?;
        // The penalty transaction has no lock time. It can follow the warning transaction at once.
        trade.broadcast_and_confirm(&penalty_tx)?;
        assert_eq!(
            trade.unspent_output(OutPoint::new(penalty_tx.compute_txid(), 0)),
            Some((escrow_amount() - fee(FORWARDING_TX_WEIGHT), 1)),
            "the other trader gets all funds minus the fees"
        );
        assert_eq!(trade.confirmations(Trade::escrow(&warning_tx)), None);
    }
    Ok(())
}

/// D1: both traders sign the custom payout with their wallets, and the transaction confirms with
/// the mediated amounts.
#[test]
fn d1_custom_payout() -> anyhow::Result<()> {
    let mut trade = Trade::with_confirmed_deposit()?;
    let sellers_amount = Amount::from_sat(3_000_000);
    // 15 sat per vbyte.
    let fee_rate = FeeRate::from_sat_per_kwu(3_750);

    for trader in [&mut trade.buyer, &mut trade.seller] {
        trader
            .model
            .sign_custom_payout(&mut trader.wallet, sellers_amount, fee_rate)?;
        assert_eq!(trader.model.state(), TradeState::CustomPayoutSigned);
    }
    let buyers_psbt = trade.buyer.model.custom_payout_psbt()?.clone();
    let sellers_psbt = trade.seller.model.custom_payout_psbt()?.clone();
    let custom_payout_tx = trade
        .seller
        .model
        .finalize_custom_payout(&mut trade.seller.wallet, Ok(buyers_psbt))?;
    let buyers_custom_payout_tx = trade
        .buyer
        .model
        .finalize_custom_payout(&mut trade.buyer.wallet, Ok(sellers_psbt))?;
    assert_eq!(
        custom_payout_tx.compute_txid(),
        buyers_custom_payout_tx.compute_txid()
    );
    let closed = TradeState::TradeClosed(ClosureType::Custom);
    assert_eq!(trade.buyer.model.state(), closed);
    assert_eq!(trade.seller.model.state(), closed);

    trade.broadcast_and_confirm(&custom_payout_tx)?;
    let txid = custom_payout_tx.compute_txid();
    let total_fee = Amount::from_sat((fee_rate.to_sat_per_kwu() * 1182).div_ceil(1000));
    let sellers_output = sellers_amount - total_fee / 2;
    let buyers_output = TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT + SELLERS_SECURITY_DEPOSIT
        - sellers_output
        - total_fee;
    assert_eq!(
        trade.unspent_output(OutPoint::new(txid, 0)),
        Some((buyers_output, 1))
    );
    assert_eq!(
        trade.unspent_output(OutPoint::new(txid, 1)),
        Some((sellers_output, 1))
    );
    assert_eq!(trade.confirmations(trade.buyer_payout), None);
    assert_eq!(trade.confirmations(trade.seller_payout), None);
    Ok(())
}

/// The bridge the protocol needs at trade closure: import the aggregated payout secret
/// together with `deposit_payout_tap_tree(..)` and the wallet ends up watching/signing exactly
/// the deposit payout output's script pubkey.
#[test]
fn imported_payout_key_matches_deposit_payout_descriptor() {
    let agg_secret = Scalar::from_slice(&[0x42; 32]).unwrap();
    let internal_key: XOnlyPublicKey =
        XOnlyPublicKey::from_slice(&agg_secret.base_point_mul().serialize_xonly()).unwrap();
    let buyer_pub_key = &"0000000000000000000000000000000000000000000000000000000000000002"
        .parse()
        .unwrap();
    let seller_pub_key = &"0000000000000000000000000000000000000000000000000000000000000003"
        .parse()
        .unwrap();

    let dir = std::env::temp_dir().join(format!("bmp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut w = BMPWallet::new(dir.as_path().into(), &test_password(), Network::Regtest).unwrap();
    let trade_wallet: &mut dyn ProtocolWalletApi = &mut w;
    trade_wallet
        .import_private_key(
            agg_secret,
            Some(deposit_payout_tap_tree(buyer_pub_key, seller_pub_key).unwrap()),
        )
        .unwrap();

    let desc = deposit_payout_descriptor(&internal_key, buyer_pub_key, seller_pub_key).unwrap();
    assert_eq!(w.imported_keys()[0].script_pubkey(), desc.script_pubkey());
    std::fs::remove_dir_all(&dir).unwrap();
}
