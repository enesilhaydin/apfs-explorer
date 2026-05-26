# M7b — APFS dosya yazma (boş dosya oluştur), scratch/sanal imaj kanıtı

Onaylı kapsam (kullanıcı): ilk dilim **root dizinde boş dosya oluştur**;
doğrulama **fsck + mount-readback + bağımsız Python decode** (üç-kat).
Gerçek-USB kapısı şimdilik atlandı (scratch güçlü oracle).

## Mevcut altyapı (M7a)

- `Transaction` (txn.rs): alloc_block / free_block / read_block / alloc_oid /
  stage_physical / stage_virtual / stage_raw / register_omap_entry / commit.
- COW snapshot create/delete (snapshot.rs) — scratch fsck-CLEAN kanıtlı.

## Baseline scratch katalog (decode edilmiş, ampirik)

- VSB: incompat=0x1 (CASE_INSENSITIVE → **hashed DREC keys**), next_obj_id=16,
  root_tree_type=0x2 (BTREE), root_tree_oid=0x404 (VIRTUAL → vol omap @xid=2 → paddr).
- fsroot kök node: flags=0x3 (ROOT|LEAF tek-node), level=0, **variable-kv**,
  toc_off=0, toc_len=64, 4 kayıt:
  - [0] DIR_REC oid=1 "private-dir" (hashed)
  - [1] DIR_REC oid=1 "root" (hashed)
  - [2] INODE oid=2 (kök dizin /, boş)
  - [3] INODE oid=3 (private-dir)
- Sıralama: birincil **obj_id artan**, ikincil **type artan**, DREC içinde
  **name-hash**.

## Hash algoritması (md.6 KIRILDI — ampirik anchor)

`apfs_name_hash(name)` = CRC32C (poly reflected 0x82F63B78, init 0xFFFFFFFF,
final-xor YOK) over **UTF-32-LE** of case-folded (lowercase, case-insensitive
volume) name; **düşük 22 bit**. Doğrulama: private-dir→0x2b29a3, root→0x2d9c79
(her ikisi de tam eşleşti). name_len_and_hash = (hash22 << 10) | (name_len incl NUL).

## On-disk struct offsetleri (apfs-core okuyucudan haritalandı)

- j_inode_val: parent_id@0, private_id@8, create@16, mod@24, change@32,
  access@40, internal_flags@48, nchildren/nlink@56(u32), default_prot@60,
  write_gen@64, bsd_flags@68, owner@72, group@76, mode@80(u16,S_IFREG=0o100000),
  pad@82, [opt uncompressed_size@84]. xfields @92: num@92(u16) used@94(u16),
  x_field_t[]={type u8,flags u8,size u16}@96, data round_up(size,8).
  INO_EXT_TYPE_NAME=4, INO_EXT_TYPE_DSTREAM=8.
- j_drec hashed key: jkey@0(type=9<<60|dir_ino), name_len_and_hash@8(u32),
  name@12 (UTF-8+NUL). val: file_id@0(u64), date_added@8(u64), flags@16(u16,
  low4=DT; DT_REG=8, DT_DIR=4).
- jkey: (type<<60)|(id&0x0FFF...). APFS_TYPE_INODE=3, DIR_REC=9, FILE_EXTENT=8.
- VSB: incompat@56, root_tree_type@116, omap_oid@128, root_tree_oid@136,
  next_obj_id@176, num_files@184, num_dirs@192.

## Görevler (TDD: kırmızı→yeşil→verify→review→commit)

1. **#58** dir-name CRC32C hash helper — unit testler bilinen değerlere pinli.
2. **#59** record builder'lar (inode val + hashed DREC key/val) — byte-layout testleri.
3. **#60** katalog tek-node insert + COW — sıralı ekleme, TOC/key/val yeniden kur,
   yeni paddr'a COW, vol omap virtual map güncelle (root_tree_oid @ yeni xid).
4. **#61** create_file orchestration — VSB next_obj_id alloc/bump, DREC+INODE
   insert, parent dir nchildren+mtime, VSB num_files, commit.
5. **#62** decisive scratch proof — fsck CLEAN + mount-readback + Python decode.

## Boş dosya için ekleme planı (ino=16, parent=2, ad="hello.txt")

- Yeni INODE: oid=16, type=3, val={parent=2, private=16, mode=S_IFREG|0644,
  nlink=1, ts=now, xfield NAME="hello.txt"}. (Boş dosya → DSTREAM xfield yok;
  fsck kabul etmezse size=0 DSTREAM eklenir — ampirik karar.)
- Yeni DREC: key={type=9, oid=2, name="hello.txt" hashed}, val={file_id=16,
  date_added=now, flags=DT_REG=8}.
- Sıralı yerleştirme: DREC oid=2/type=9 → [2] INODE(oid2) sonrası, [3] öncesi.
  INODE oid=16 → en sona.
- Parent (kök dizin ino=2) inode val: nchildren += 1, mod/change_time=now.
- VSB: next_obj_id=17, num_files += 1.
- fsroot tek-node COW → yeni paddr; vol omap'a (root_tree_oid, new_xid)→paddr.

## Doğrulama oracle

scratch test snapshot_scratch.rs harness'ını yansıt: gen-scratch → create_file
→ fsck CLEAN → hdiutil mount + dosya görünür/oku → bağımsız Python decode DREC+
INODE bulur. False-green imkânsız.
