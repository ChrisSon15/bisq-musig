# Derivation of the key shares

Each trader holds two secret key shares per trade: his share of `P'` (the key of the buyer's
payout) and his share of `Q'` (the key of the seller's payout). This file defines how they are
derived from the wallet seed.

## Requirements

- The same seed and the same trade ID always give the same two key shares.
- Different trade IDs give unrelated key shares.
- A key share is never a key of the BIP86 wallet, and it cannot be computed from any public key
  of the wallet.
- A key share that is given to the peer reveals nothing about the seed, about the trader's other
  key shares, or about the keys of the wallet.
- The seed does not leave the wallet. The wallet offers one operation that returns the key shares
  for a trade ID.
- Key shares are not stored. They are derived again when they are needed.

Rationale: random key shares exist only in the memory or in the store of the engine. If they are
lost after the deposit is confirmed, the trader cannot sign anything for this trade any more.
Derived key shares can be restored from the seed and the trade ID.

## Rule

1. **Trade root secret.** The private key at the BIP32 path `m/86'/coin_type'/0'/2'` of the wallet
   seed, as 32 bytes. `coin_type` is 0 on mainnet and 1 on all other networks, as in the wallet.
   All four steps of the path are hardened.
2. **Message.** The concatenation of:
   - the trade root secret, 32 bytes;
   - the length of the trade ID in bytes, as a 4-byte big-endian number;
   - the trade ID, encoded as UTF-8;
   - one label byte: `0x00` for the share of `P'` (buyer's payout), `0x01` for the share of `Q'`
     (seller's payout).
3. **Hash.** The tagged hash of the message as defined in BIP340, with the tag
   `BisqMuSig/key-share`: `SHA256(SHA256(tag) || SHA256(tag) || message)`.
4. **Key share.** The hash, read as a big-endian number, reduced modulo the order of the
   secp256k1 group. If the result is zero, the trade ID is rejected.

The label depends on the payout, not on the role of the trader. A buyer and a seller with the same
seed and trade ID would derive the same two key shares.

Rationale for the path: the BIP86 wallet uses only the children 0 and 1 of the account key
`m/86'/coin_type'/0'`, and both are not hardened. The hardened child 2 is not used by the wallet,
and it cannot be computed from the public key of the account. Rationale for the hash: the output
of a hash reveals nothing about its input, so a key share that the peer learns does not help to
find the trade root secret or another key share.

The path and the tag are open point O6 in [README.md](README.md).

## Trade IDs

Because the key shares depend only on the seed and the trade ID, a trade ID must never be used for
two trades of the same wallet. The engine accepts a trade ID only once, also after the trade with
this ID is closed ([persistence-and-recovery.md](persistence-and-recovery.md)).

Rationale: a second trade with the same ID would have the same key shares. A key share that was
given to the peer of the first trade would then be known for the second trade.

## Nonces

The nonces of the MuSig2 signatures are not derived. They are random, they exist only in memory,
and they are never stored. A nonce is used for exactly one signature.

Rationale: a nonce that is used for two different messages reveals the key share. A derived or
stored nonce could be used again after a restart or a restore.

## Test vectors

Mnemonic (BIP39, without passphrase):

```text
abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about
```

Trade root secret:

| Path | Trade root secret |
|---|---|
| `m/86'/1'/0'/2'` | `1080e8985042bd55a83cf460a238642df96f0b46d69c67872b7e4bdb019d4d64` |
| `m/86'/0'/0'/2'` | `a339f9d6eeecf26652a83a90327cdb99eb9f23e2de4338fb25a3f403701efdaf` |

Key shares for `coin_type` 1:

| Trade ID | Label | Secret key share | Public key share |
|---|---|---|---|
| `trade-1` | `0x00` | `b2791aa0c44ba3f99274c5faffd9a5ecfb2a04081b811d81a39b5f1ca63a9c86` | `0365af31fa475be070b752da8652cfd92cbe6c5d80070a1ddfedb821ff2435a576` |
| `trade-1` | `0x01` | `5ad667b8870bf171342cc66d1048ab0f8aa563bb9a22b5e6a5700e839ca85770` | `02eeb60b61906388346cf8caa2fe5a79596dd3fac1e047f79c9ae8cf577bc361ba` |
| `trade-2` | `0x00` | `83ccc10bc994823b37de146604a3e1ae78ae24340b1692f0164ceaca92414a8a` | `03a672e62c51e14078f3508c53d7b40829febf3aacc081b4adfb76757ce874a115` |
| `trade-2` | `0x01` | `c7fa6364f66f22102b42b4017aef6b03bbad42b1a20d08330fa0d28f8471155e` | `03fd2089b1e906ac40656964072a43a488171724a59ab1d0e2e7ecafd5279d9460` |

Key shares for `coin_type` 0:

| Trade ID | Label | Secret key share | Public key share |
|---|---|---|---|
| `trade-1` | `0x00` | `9cc8073b223e4cc4dbb5223da35ee5c2e79ed293a586cbb3a2e52bd2346fe7e4` | `024efb07047fb747611c81bbd2cd5706282f7ea71a051836a2475fbba28d74478f` |
| `trade-1` | `0x01` | `a92f8710f192377d086ae52ce0145cf98a3a43a9a6d667803eba9aaf43e63fa1` | `021ceeaf87477ace942b0e0eacc8c2fabe39b04a5cc30729d5a347062f95a1a3a5` |
| `trade-2` | `0x00` | `ebf246e7bd1d169bf652110616a5fa29e6e8e22e2f06feca9adf381d7e684674` | `035a8189e2549b1de0be14462952c11bf72977f9cab96726f3748219d3d7187c5c` |
| `trade-2` | `0x01` | `e8cbd0e40b2a3e95ff5810057730d5ed7803bb644737ffa080ed4fce8688c38a` | `0383d38dfcea5820febbb13c5594d49d480d55904bcd4a1545e5c3f5cd3605e250` |

The vectors were computed with [key-derivation-vectors.py](key-derivation-vectors.py). The script
checks its own building blocks against published test vectors of BIP32, BIP39, BIP86 and BIP340
before it prints the values. The vectors are not yet checked against an implementation in the
wallet, because that implementation does not exist yet.
