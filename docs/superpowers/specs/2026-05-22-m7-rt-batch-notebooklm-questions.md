# M7a-RT + ileri-iş toplu NotebookLM soru seti (2026-05-22)

> Amaç: limit varken **tüm genuine belirsizlikleri** tek seferde sor. Her soru
> kopyala-yapıştır hazır. Yanıt gelince ilgili `apfs-spec:*` KB'ye kilitle.
> md.6: NotebookLM/Apple PDF yalnız HİPOTEZ; nihai hakem gerçek macOS diski.
>
> Durum etiketleri:
> - **[ACİL-TEYİT]** ampirik düzelttim, kuralın genelini teyit istiyorum
> - **[ACİL-AÇIK]** şu an gereken, ampirik çözemedim
> - **[İLERİ]** sonraki milestone'lar için önden topluyorum
> - **[BİLİNEN]** ampirik cevabım var, sadece doğrulatma

---

## A. B-tree node TOC / `btn_table_space` ayırma kuralı

Bağlam (ampirik bulgu): Kernel'in taze formatladığı **boş** extentref kök
node'u `btn_table_space.off=0, len=64` (8 kvloc slotu) + `btn_free_space.off=0,
len=3936` taşıyor. Bizim 1 kayıtlı node'umuz `table_space.len=8` ve fsck kabul
ediyor. Ama son kayıt silinip node boşalınca `table_space.len=0` ürettiğimizde
fsck "Extent ref tree is invalid" diyor. Boş node'u kernel'in 64-bayt düzenine
byte-tam eşleyince CLEAN oluyor.

1. **[ACİL-TEYİT]** APFS `btree_node_phys_t` içinde `btn_table_space`
   (table-of-contents alanı) tam olarak nasıl boyutlanır? Sıkı mı
   (nkeys × entry_size) yoksa sabit granüllerle mi (örn. 8 slotluk 64-bayt
   bloklar) ayrılır?

2. **[ACİL-TEYİT]** Geçerli bir node için `btn_table_space.len` için bir
   ALT SINIR var mı? `len=0` (sıfır TOC kapasitesi) boş bir node için hiç
   geçerli olabilir mi, yoksa boş bir b-tree kök node'u DAİMA sıfır-olmayan bir
   TOC alanı (kernel'de 64) ayırmak zorunda mı?

3. **[ACİL-AÇIK]** Kayıtlar silinip node boşalınca kernel `table_space`'i
   tekrar 0'a doğru küçültür mü, önceki kapasiteyi korur mu, yoksa varsayılana
   (64) sıfırlar mı? APFS, repack yerine `btn_table_space` + `key_free_list`/
   `val_free_list` ile boşalan TOC slotlarını izliyor olabilir mi?

4. **[İLERİ]** TOC kapasitesi (`table_space.len`) hiç 2'nin kuvveti / hizalı
   olmak zorunda mı? Leaf vs internal node arasında, ya da fixed-kv vs
   variable-kv (KV_NONALIGNED) ağaçlar arasında bu kural değişir mi?

---

## B. Spaceman: CIB / bitmap / internal-pool / free-queue COW

Bağlam: Allocator gerçek persistent bitmap'i şu an **in-place** (`dev.write_at`)
güncelliyor; COW değil. 32GB USB'de tekrarlı yazımlarda muhasebe sapması
gözlendi (M7a-RT #56). 64MB scratch hep `bitmap_addr=0` implicit chunk
kullandığı için bu yol hiç tetiklenmiyordu; 2GB scratch'te chunk 0 gerçek bitmap
taşıyor (bunu artık tekrarlanabilir kıldık).

5. **[ACİL-AÇIK]** Bir transaction içinde blok ayırırken/serbest bırakırken,
   per-chunk allocation bitmap bloğu (`chunk_info.ci_bitmap_addr`) copy-on-write
   olmak ZORUNDA mı (yeni bitmap bloğu ayır, içeriği kopyala+değiştir,
   `ci_bitmap_addr`'ı güncelle), yoksa mevcut bitmap bloğunu yerinde değiştirmek
   doğru mu?

6. **[ACİL-AÇIK]** `chunk_info_block` (CIB) blokları ve per-chunk bitmap blokları
   fiziksel olarak nerede yaşar — spaceman "internal pool"unda mı
   (`sm_ip_base` / `sm_ip_block_count`) yoksa normal ayrılabilir chunk
   alanında mı? Internal pool'un kendi serbest/dolu durumu nasıl izlenir
   (`sm_ip_bitmap` / `sm_ip_bm_*`) ve yeni bir bitmap/CIB bloğu ayırmak
   internal-pool bitmap'inden mi geçmeli?

7. **[ACİL-AÇIK]** `chunk_info.ci_xid`'in rolü nedir? Bir chunk'ın bitmap'i
   değiştirildiğinde mevcut transaction xid'ine bump edilmek zorunda mı? fsck,
   bir chunk'a en son hangi checkpoint'in dokunduğunu doğrulamak için `ci_xid`'i
   kullanır mı?

8. **[ACİL-AÇIK]** Spaceman free queue (`spaceman_free_queue` / `sm_fq` ağaçları)
   ERTELENMİŞ serbest bırakma için nasıl çalışır? Bir COW işlemi eski bir bloğu
   (eski bitmap, eski CIB, eski b-tree node, eski omap) serbest bırakırken bunu
   bitmap'te hemen temizlemek yerine free queue'ya mevcut xid ile EKLEMEK
   zorunda mı? fsck'in "space manager free queue trees" kontrolü neyi doğrular?

9. **[BİLİNEN→teyit]** Container'da iki spaceman device var
   (`sm_dev[0]`=main, `sm_dev[1]`). Tek-volume container için `sm_dev[1]` daima
   boş mu (`cib_count=0`)?

10. **[ACİL-AÇIK]** Yeni blok ayırıp canlı bitmap'te bit set ettiğimizde, önceki
    checkpoint'in spaceman'i (hâlâ descriptor ring'de) daha yüksek bir
    `sm_free_count` raporlar. Şu an eski spaceman'in `sm_free_count`'unu
    eşleştirmek için yamanıyoruz. Bu doğru mu, yoksa APFS bitmap'in paylaşımlı
    olmasına + free queue'ya mı dayanıp eski spaceman free_count'larına hiç
    dokunmaz mı? fsck "overallocation detected on Main device"i tam olarak ne
    tetikler (çift-referans mı, bitmap-vs-extent uyuşmazlığı mı)?

---

## C. Extentref / katalog ağacı büyüme & çok-node

11. **[BİLİNEN→teyit]** Extentref ağacı (`extentref_tree_oid`) volume
    superblock'ta DOĞRUDAN fiziksel paddr olarak tutuluyor (ampirik:
    `o_oid == paddr`), katalog gibi omap-üzerinden virtual DEĞİL. Doğru mu?

12. **[İLERİ]** Extentref ağacı tek node'u aşıp split olunca, internal-node
    child pointer'ları FİZİKSEL paddr mı yoksa VIRTUAL oid mi (katalog/fsroot
    ağacının omap-üzerinden virtual oid kullanmasının aksine)? Pivot anahtarı
    yine child'ın ilk anahtarının tam kopyası mı?

13. **[İLERİ]** Katalog (fsroot) ağacını mutasyon sonrası bottom-up yeniden
    kurarken her node her transaction'da YENİ virtual oid almak zorunda mı,
    yoksa node oid'leri transaction'lar arası sabit kalıp (aynı virtual oid,
    sadece omap paddr'ı değişir) olabilir mi? fsck bunu umursar mı?

---

## D. İleri op semantiği (rmdir / truncate / hardlink / sparse)

14. **[İLERİ]** rmdir: tam boşluk kontrolü nedir (sadece "." örtük mü, yoksa
    sıfır DIR_REC çocuk şartı mı)? Hangi kayıtlar silinmeli (inode, drec, ve
    varsa xfield'lar, dir-stats)?

15. **[İLERİ]** Bir dosyayı truncate/extend ederken `FILE_EXTENT` kayıtları ve
    dstream (`ds_size`, `ds_alloced_size`, `total_bytes_written`) nasıl
    güncellenir? Bir DELİK (sparse) için `phys_block_num=0` olan bir
    `FILE_EXTENT` yayılmak zorunda mı ve extentref refcount delikleri nasıl
    işler?

16. **[İLERİ]** Hard link'ler APFS'te `SIBLING_LINK` (type 12) +
    `SIBLING_MAP` (type 13) + birincil DREC ile nasıl temsil edilir? İlişki tam
    olarak nedir ve inode'un `nlink` alanı nasıl güncellenir?

17. **[İLERİ]** Normal (sıkıştırılmamış) bir dosya için `j_inode_val`
    `internal_flags` / `uncompressed_size` ne olmalı? fsck `ds_size ==
    Σ(FILE_EXTENT uzunlukları)` eşitliğini doğrular mı?

---

## Kapanış notu

A grubu (TOC) bu oturumda ampirik çözüldü (kernel boş-node = 64-bayt TOC,
byte-tam eşlendi, scratch fsck CLEAN); 1-4 yalnız kuralın genelini kilitlemek
için. B grubu (spaceman COW) = M7a-RT #56 çekirdeği, gerçek-USB final-proof'un
kalan tek bilinmeyeni. C/D = sonraki milestone'lar için önden.

---

## YANITLAR (KİLİTLİ — NotebookLM + Apple PDF, 2026-05-22)

> md.6: bunlar HİPOTEZ; nihai hakem gerçek macOS diski. A grubu zaten gerçek
> diskle (kernel boş-node) doğrulandı + scratch fsck CLEAN. B grubu uygulanınca
> gerçek-USB ile sınanacak.

### A. B-tree TOC
- **1-2:** Spec'te `len=0` katı yasağı yok, AMA `fsck` fixed-kv kernel ağaçları
  (EXTENTREF, OMAP) için boş node'da bile **önceden ayrılmış TOC kapasitesi**
  bekler. Kernel format: extentref boş = **64 byte**, omap boş = **448 byte**.
  (PDF s.128, s.131: "allocates enough space for the TOC to avoid expanding").
  → Bizim `EMPTY_TOC_CAP=64` düzeltmemiz DOĞRU.
- **3:** Kernel boşalan node'da `table_space`'i 0'a küçültmez; orijinal
  kapasiteyi KORUR, sadece `key/val_free_list`'i günceller.
- **4:** `table_space.len` daima giriş boyutunun katı (kvloc 8B / kvoff 4B).
  Variable-kv (katalog) esnek (nkeys*8) büyür; fixed-kv önceden maks/granül.

### B. Spaceman COW (ANA İŞ — M7a-RT #56)
- **5: bitmap COW ZORUNLU.** Aktif checkpoint'in fiziksel bloğunun üzerine
  yazmak YASAK. `ci_bitmap_addr` gerçek bloksa: (1) yeni blok tahsis et,
  (2) eski bitmap'i kopyala+biti değiştir, (3) `ci_bitmap_addr`'ı yeni paddr'a
  güncelle → bu CIB bloğunun da COW olmasını tetikler.
- **6:** Bitmap + CIB blokları **NORMAL allocable alanda** yaşar (internal
  pool'da DEĞİL). Internal pool (`sm_ip_base`, PDF s.69) yalnız spaceman'in
  kendi metadata/free-queue ağaçlarını bootstrap'ı için.
- **7:** `ci_xid` = chunk'ın son güncellendiği xid (PDF s.72). Bitmap COW'da
  mevcut xid'e BUMP edilmek zorunda; fsck stale-data kontrolünü bununla yapar.
- **8:** COW'lanan eski blok (eski bitmap/CIB/omap/btree node) o işlemde free
  EDİLEMEZ (eski checkpoint hâlâ referans ediyor) → `spaceman_free_queue`'ya
  mevcut xid ile eklenir, sonraki checkpoint'te reaper temizler. fsck "free
  queue trees" kontrolü: kuyruktakilerin xid'i geçmiş + canlı referans yok.
- **9:** `sm_dev[1]` tek-volume container'da daima boş (cib_count=0). ✓
- **10:** Eski spaceman'i yamamak **YANLIŞ** (checkpoint'ler immutable).
  `overallocation detected` tam da in-place bitmap update / eski checkpoint'in
  referans ettiği bloğu free işaretlemekten gelir. Bitmap COW + free-queue →
  hata kaybolur. **→ mevcut `sync_prev_spaceman_free_count` + in-place
  `dev.write_at` bitmap kaldırılacak.**

### C. Ağaç büyüme
- **11:** `extentref_tree_oid` (VSB) doğrudan **fiziksel paddr** (PDF s.78),
  omap'te aranmaz. ✓ (ampirik doğrulandı: o_oid==paddr)
- **12:** Extentref fiziksel ağaç → split'te internal child pointer'ları da
  **fiziksel paddr (8B)**, virtual oid DEĞİL.
- **13:** Katalog node COW'da **YENİ virtual oid ALMAZ** — orijinal oid'i korur,
  yalnız omap `{oid,xid}->paddr` eşlemesi yeni paddr'a güncellenir. Split'te
  SOL node oid'i korur, yalnız YENİ (sağ) node `nx_next_oid`'den yeni oid alır.
  (Not: mevcut rewrite_fstree her node'a yeni oid veriyor — fsck soundness'i
  umursuyor, kabul ediyor; bu optimizasyon/uyum için ileride gözden geçirilir.)

### D. İleri op
- **14: rmdir:** çocuk `DIR_REC` (tür9) = 0 ise boş. Sil: INODE(3) + üst DIR_REC
  + xfield'lar (DIR_STATS). Üst INODE nchildren -= 1.
- **15: sparse:** delik için FILE_EXTENT yazılmaz (logical_addr atlar);
  phys=0 mantıksal extent gerekmez. Dstream size = mantıksal EOF. Extentref
  tahsis edilmemiş bloklar için refcount tutmaz.
- **16: hardlink:** `SIBLING_LINK`(12) + `SIBLING_MAP`(13) + birincil INODE;
  `nlink` = işaret eden dizin sayısı (PDF s.100-101).
- **17:** Normal dosya `uncompressed_size=0` (yalnız decmpfs kullanır). fsck
  `j_dstream.alloced_size == Σ(FILE_EXTENT uzunlukları)` eşitliğini doğrular.
