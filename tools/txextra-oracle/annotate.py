import sys
sys.path.insert(0, __import__('os').path.dirname(__file__) or '.')
from model import read_varint
def ann(name, hexs):
    b=bytes.fromhex(hexs); n=len(b); pos=0
    print(f'--- {name}  ({n} bytes)')
    while pos<n:
        t=b[pos]; start=pos; pos+=1
        if t==0x00:
            z=0
            while pos<n and b[pos]==0: z+=1; pos+=1
            print(f'  @{start:3d} 00                 PADDING  tag+{z} zero bytes -> tx_extra_padding.size={1+z}')
        elif t==0x01:
            print(f'  @{start:3d} 01 {b[pos:pos+32].hex()}  PUBKEY'); pos+=32
        elif t in (0x02,0xDE):
            st,nr,l,pos=read_varint(b,pos)
            data=b[pos:pos+l]; pos+=l
            kind='NONCE' if t==0x02 else 'MINERGATE'
            extra=''
            if t==0x02 and l==33 and data[0]==0x00: extra=f'  => PAYMENT_ID(32) {data[1:].hex()}'
            if t==0x02 and l==9 and data[0]==0x01: extra=f'  => ENCRYPTED_PAYMENT_ID(8) {data[1:].hex()}'
            asc=''.join(chr(c) if 32<=c<127 else '.' for c in data)
            print(f'  @{start:3d} {t:02x} len={l:<4d} {kind}  data={data.hex()}{extra}')
            if t==0x02 and not extra: print(f'        ascii="{asc}"')
        elif t==0x03:
            st,nr,l,pos=read_varint(b,pos)
            inner=b[pos:pos+l]; pos+=l
            ist,inr,depth,ip=read_varint(inner,0)
            print(f'  @{start:3d} 03 len={l:<4d} MERGE_MINING depth={depth} merkle_root={inner[ip:ip+32].hex()} (inner fully consumed: {ip+32==len(inner)})')
        elif t==0x04:
            st,nr,c,pos=read_varint(b,pos)
            keys=[b[pos+32*i:pos+32*i+32].hex() for i in range(c)]; pos+=32*c
            print(f'  @{start:3d} 04 cnt={c} ADDITIONAL_PUBKEYS {keys}')
        else:
            print(f'  @{start:3d} {t:02x} UNKNOWN TAG -> parse aborts here, fields so far are kept'); break
ann('testnet tx 2917a83ec6.. (block 134721, v1)','01495bbb2d69001caf7dfd13b662f3ea1b7c247e68b10750418652d5f3909c98d2')
ann('testnet coinbase block 134721','0115a1f4a3913414d73640baaf6498af7c55bafb418b7aff003a240d759352cfac')
ann('mainnet coinbase block 500000','012aaee37ea173229ab552f6b7e5fb9870f412dc6fa853d443eb3a48e40a983b90021142cb6a000000000000000000000000000003210165fd83daffbd2b088496463ed4cdc508b0dfd5a6610f22b47739136c073e918b')
ann('mainnet coinbase block 73060 (MinerGate)','01d4f881f253632053f7858c837ed526acd25c89c5506ed6049a53b993a656aa660262b1bd5c00204d696e657247617465273932200066050000000000eaf02568c5664b00ffffffff000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000032100ff9c2be08c0fdfad7fcf0f0a52d6ed3c11e7f5152a46def341343933e8ef4cc500000000000000000000')
ann('mainnet tx e8256130da.. (32-byte payment id, nonce FIRST)','0221002715536cb0e7c24faeb02b6659dbdae5d701da922cc1034846695b72157a4b650156268a40135e84463e9708968924dc93b9aaa2954b462f949f69c7363c2f43f9')
ann('mainnet tx 32ee937d78.. (encrypted payment id)','01455fe610dfc1b1efec57b39885c28ac08defac0291ae19349a354d159473e00a02090155e10418dec2fd58')
ann('mainnet tx f0d450ad91.. (0xDE minergate)','010397a0d0b5b7a4f247007475dc6e3d02fba96de663a461f174904b84ec08a9e2de206de0332d02832042ee8b7d7839bfc10d3b4e38307ea1cc14825b1fcac2df1021')
ann('mainnet coinbase block 61190 (pubkey + padding)','010511d7abbca8479bf7d9569b938c3edb8b06c31640de817c321ea0d0a35e3614000000000000000000000000000000000000000000000000000000000000000000000000000000')
