"""Computes the test vectors of key-derivation.md.

The script has no dependencies. It checks its building blocks against published test vectors of
BIP32, BIP39, BIP86 and BIP340 before it prints the values.
"""
import hashlib, hmac, struct

P = 2**256 - 2**32 - 977
N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
G = (0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798,
     0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8)

def add(a, b):
    if a is None: return b
    if b is None: return a
    if a[0] == b[0] and (a[1] + b[1]) % P == 0: return None
    if a == b: l = 3 * a[0] * a[0] * pow(2 * a[1], -1, P) % P
    else: l = (b[1] - a[1]) * pow(b[0] - a[0], -1, P) % P
    x = (l * l - a[0] - b[0]) % P
    return (x, (l * (a[0] - x) - a[1]) % P)

def mul(k, pt=G):
    r = None
    while k:
        if k & 1: r = add(r, pt)
        pt = add(pt, pt); k >>= 1
    return r

def compressed(k):
    x, y = mul(k)
    return bytes([2 + (y & 1)]) + x.to_bytes(32, 'big')

def master(seed):
    i = hmac.new(b'Bitcoin seed', seed, hashlib.sha512).digest()
    return int.from_bytes(i[:32], 'big'), i[32:]

def ckd_hardened(k, c, index):
    data = b'\x00' + k.to_bytes(32, 'big') + struct.pack('>I', index | 0x80000000)
    i = hmac.new(c, data, hashlib.sha512).digest()
    il = int.from_bytes(i[:32], 'big')
    assert il < N
    child = (il + k) % N
    assert child != 0
    return child, i[32:]

def tagged_hash(tag, msg):
    t = hashlib.sha256(tag.encode()).digest()
    return hashlib.sha256(t + t + msg).digest()

# --- self checks -------------------------------------------------------------------------
# BIP32 test vector 1: seed 000102...0f, chain m/0H -> private key edb2e14f...
k, c = master(bytes(range(16)))
assert k.to_bytes(32, 'big').hex() == 'e8f32e723decf4051aefac8e2c93c9c5b214313817cdb01a1494b917c8436b35'
k0, c0 = ckd_hardened(k, c, 0)
assert k0.to_bytes(32, 'big').hex() == 'edb2e14f9ee77d26dd93b4ecede8d16ed408ce149b6cd80b0715a2d911a0afea'
assert c0.hex() == '47fdacbd0f1097043b78c63c20c34ef4ed9a111d980047ad16282c7ae6236141'
# secp256k1: public key of private key 1 and of BIP340 test vector 0 (key 3).
assert compressed(1).hex() == '0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798'
assert compressed(3)[1:].hex().upper() == 'F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9'
# BIP340 tagged hash: the tag hash of "BIP0340/challenge" is a published constant.
assert hashlib.sha256(b'BIP0340/challenge').hexdigest() == '7bb52d7a9fef58323eb1bf7a407db382d2f3f2d81bb1224f49fe518f6d48d37c'
# BIP39: seed of the "abandon ... about" mnemonic without passphrase (published test vector).
MNEMONIC = ' '.join(['abandon'] * 11 + ['about'])
seed = hashlib.pbkdf2_hmac('sha512', MNEMONIC.encode(), b'mnemonic', 2048)
assert seed.hex().startswith('5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc1')
# BIP86 test vector: the account key m/86'/0'/0' of the same mnemonic.
BIP86_ACCOUNT_XPRV = ('xprv9xgqHN7yz9MwCkxsBPN5qetuNdQSUttZNKw1dcYTV4mkaAFiBVGQziHs3NRSWMkCzvgjEe3n9'
                      'xV8oYywvM8at9yRqyaZVz6TYYhX98VjsUk')
BASE58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
number = 0
for character in BIP86_ACCOUNT_XPRV: number = number * 58 + BASE58.index(character)
raw = number.to_bytes(82, 'big')
assert hashlib.sha256(hashlib.sha256(raw[:78]).digest()).digest()[:4] == raw[78:]
k, c = master(seed)
for index in (86, 0, 0): k, c = ckd_hardened(k, c, index)
assert k.to_bytes(32, 'big') == raw[46:78] and c == raw[13:45]
print('self checks passed')

TAG = 'BisqMuSig/key-share'
def trade_root(seed, coin_type):
    k, c = master(seed)
    for index in (86, coin_type, 0, 2):
        k, c = ckd_hardened(k, c, index)
    return k

def key_share(root, trade_id, label):
    tid = trade_id.encode('utf-8')
    msg = root.to_bytes(32, 'big') + struct.pack('>I', len(tid)) + tid + bytes([label])
    s = int.from_bytes(tagged_hash(TAG, msg), 'big') % N
    assert s != 0
    return s

# Worked example with all intermediate values.
example_root = trade_root(seed, 1)
example_id = 'trade-1'.encode('utf-8')
example_message = example_root.to_bytes(32, 'big') + struct.pack('>I', len(example_id)) + example_id + bytes([0])
print('worked example (coin type 1, trade ID "trade-1", label 00)')
print('  tag hash:', hashlib.sha256(TAG.encode()).hexdigest())
print('  message:', example_message.hex())
print('  tagged hash:', tagged_hash(TAG, example_message).hex())
print('  key share:', key_share(example_root, 'trade-1', 0).to_bytes(32, 'big').hex())
print('mnemonic:', MNEMONIC)
print('seed:', seed.hex())
for coin_type, name in ((1, "m/86'/1'/0'/2' (testnet, signet, regtest)"), (0, "m/86'/0'/0'/2' (mainnet)")):
    root = trade_root(seed, coin_type)
    print(name, 'trade root secret:', root.to_bytes(32, 'big').hex())
    for trade_id in ('trade-1', 'trade-2'):
        for label, lname in ((0, 'buyer payout'), (1, 'seller payout')):
            s = key_share(root, trade_id, label)
            print(f'  {trade_id} | {lname} ({label:02x}) | {s.to_bytes(32,"big").hex()} | {compressed(s).hex()}')
